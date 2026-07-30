use std::borrow::Cow;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_install_context::distribution::PRODUCT_NAME;
use codex_install_context::distribution::SPINE_FEEDBACK_SENTRY_DSN;
use reqwest::header::CONTENT_TYPE;
use sentry::protocol::Attachment;
use sentry::protocol::AttachmentType;
use sentry::protocol::Envelope;
use sentry::protocol::EnvelopeItem;
use sentry::protocol::Event;
use sentry::protocol::Level;
use sentry::types::Dsn;

use crate::FeedbackAttachment;

/// Fixed filename for the content-redacted Spine rollout attachment.
pub const SPINE_ROLLOUT_DEBUG_ATTACHMENT_FILENAME: &str = "rollout-debug.jsonl.gz";
/// Maximum UTF-8 byte length of an optional Spine feedback note.
pub const SPINE_FEEDBACK_MAX_NOTE_BYTES: usize = 8 * 1024;
/// Maximum combined byte length of all Spine feedback attachments.
pub const SPINE_FEEDBACK_MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

const SPINE_FEEDBACK_UPLOAD_TIMEOUT: Duration = Duration::from_secs(10);
const SPINE_FEEDBACK_ENVELOPE_OVERHEAD_BYTES: usize = 64 * 1024;
const SPINE_FEEDBACK_EVENT_MESSAGE: &str = "SpineCodex feedback";
const SPINE_FEEDBACK_LOGGER: &str = "spinecodex.feedback";
const SPINE_FEEDBACK_CONTENT_TYPE: &str = "application/x-sentry-envelope";
const ROLLOUT_DEBUG_CONTENT_TYPE: &str = "application/gzip";
const SCREENSHOT_CONTENT_TYPE: &str = "image/png";
const SCREENSHOT_FILENAMES: [&str; 3] =
    ["screenshot-1.png", "screenshot-2.png", "screenshot-3.png"];

/// Inputs for one checked SpineCodex feedback upload.
pub struct SpineFeedbackUpload<'a> {
    /// Optional user-authored feedback note.
    pub note: Option<&'a str>,
    /// Validated in-memory rollout and screenshot attachments.
    pub attachments: &'a [FeedbackAttachment],
}

struct SpineFeedbackTransportConfig<'a> {
    dsn: &'a str,
    timeout: Duration,
    disable_proxy: bool,
}

/// Submit one SpineCodex feedback envelope and return its Sentry event ID.
pub fn upload_spine_feedback(options: SpineFeedbackUpload<'_>) -> Result<String> {
    upload_spine_feedback_with_config(
        options,
        SpineFeedbackTransportConfig {
            dsn: SPINE_FEEDBACK_SENTRY_DSN,
            timeout: SPINE_FEEDBACK_UPLOAD_TIMEOUT,
            disable_proxy: false,
        },
    )
}

fn upload_spine_feedback_with_config(
    options: SpineFeedbackUpload<'_>,
    config: SpineFeedbackTransportConfig<'_>,
) -> Result<String> {
    validate_note(options.note)?;
    validate_attachments(options.attachments)?;

    let dsn = Dsn::from_str(config.dsn).context("invalid Spine feedback DSN")?;
    let mut event = Event {
        level: Level::Info,
        message: Some(SPINE_FEEDBACK_EVENT_MESSAGE.to_string()),
        logger: Some(SPINE_FEEDBACK_LOGGER.to_string()),
        release: Some(Cow::Owned(format!(
            "{PRODUCT_NAME}@{}",
            env!("CARGO_PKG_VERSION")
        ))),
        tags: BTreeMap::from([
            (
                "cli_version".to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
            ),
            (
                "feedback_kind".to_string(),
                "spine_rollout_debug".to_string(),
            ),
            ("product".to_string(), PRODUCT_NAME.to_string()),
        ]),
        ..Event::new()
    };
    if let Some(note) = options.note {
        event.extra.insert(
            "note".to_string(),
            serde_json::Value::String(note.to_string()),
        );
    }
    let event_id = event.event_id;

    let mut envelope = Envelope::new();
    envelope.add_item(EnvelopeItem::Event(event));
    for attachment in options.attachments {
        envelope.add_item(EnvelopeItem::Attachment(Attachment {
            buffer: attachment.buffer.clone(),
            filename: attachment.filename.clone(),
            content_type: attachment.content_type.clone(),
            ty: Some(AttachmentType::Attachment),
        }));
    }

    let mut body = Vec::new();
    envelope
        .to_writer(&mut body)
        .context("serialize Spine feedback envelope")?;
    if body.len() > SPINE_FEEDBACK_MAX_ATTACHMENT_BYTES + SPINE_FEEDBACK_ENVELOPE_OVERHEAD_BYTES {
        bail!("Spine feedback envelope is too large: {} bytes", body.len());
    }

    let mut client_builder = reqwest::blocking::Client::builder().timeout(config.timeout);
    if config.disable_proxy {
        client_builder = client_builder.no_proxy();
    }
    let client = client_builder
        .build()
        .context("build Spine feedback HTTP client")?;
    let auth = dsn
        .to_auth(Some(&format!(
            "{PRODUCT_NAME}/{}",
            env!("CARGO_PKG_VERSION")
        )))
        .to_string();
    let response = client
        .post(dsn.envelope_api_url())
        .header("X-Sentry-Auth", auth)
        .header(CONTENT_TYPE, SPINE_FEEDBACK_CONTENT_TYPE)
        .body(body)
        .send()
        .context("submit Spine feedback envelope")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Spine feedback ingest returned HTTP {status}");
    }

    Ok(event_id.simple().to_string())
}

fn validate_note(note: Option<&str>) -> Result<()> {
    if note.is_some_and(|note| note.len() > SPINE_FEEDBACK_MAX_NOTE_BYTES) {
        bail!("Spine feedback note exceeds {SPINE_FEEDBACK_MAX_NOTE_BYTES} bytes");
    }
    Ok(())
}

fn validate_attachments(attachments: &[FeedbackAttachment]) -> Result<()> {
    let mut rollout_seen = false;
    let mut screenshots_seen = [false; SCREENSHOT_FILENAMES.len()];
    let mut total_bytes = 0_usize;

    for attachment in attachments {
        total_bytes = total_bytes
            .checked_add(attachment.buffer.len())
            .context("Spine feedback attachment size overflow")?;
        if total_bytes > SPINE_FEEDBACK_MAX_ATTACHMENT_BYTES {
            bail!("Spine feedback attachments exceed {SPINE_FEEDBACK_MAX_ATTACHMENT_BYTES} bytes");
        }

        match attachment.filename.as_str() {
            SPINE_ROLLOUT_DEBUG_ATTACHMENT_FILENAME => {
                if rollout_seen {
                    bail!("duplicate rollout debug attachment");
                }
                require_content_type(attachment, ROLLOUT_DEBUG_CONTENT_TYPE)?;
                rollout_seen = true;
            }
            filename => {
                let Some(index) = SCREENSHOT_FILENAMES
                    .iter()
                    .position(|candidate| *candidate == filename)
                else {
                    bail!("unapproved Spine feedback attachment filename");
                };
                if screenshots_seen[index] {
                    bail!("duplicate Spine feedback screenshot attachment");
                }
                require_content_type(attachment, SCREENSHOT_CONTENT_TYPE)?;
                screenshots_seen[index] = true;
            }
        }
    }

    if !rollout_seen {
        bail!("missing rollout debug attachment");
    }
    Ok(())
}

fn require_content_type(attachment: &FeedbackAttachment, expected: &str) -> Result<()> {
    if attachment.content_type.as_deref() != Some(expected) {
        bail!(
            "invalid content type for Spine feedback attachment {}",
            attachment.filename
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    use super::super::FeedbackAttachment;
    use super::SpineFeedbackTransportConfig;
    use super::SpineFeedbackUpload;
    use super::upload_spine_feedback_with_config;
    use serde_json::Value;

    const TEST_DSN_PREFIX: &str = "http://public-key@";

    fn attachment(filename: &str, content_type: &str, buffer: &[u8]) -> FeedbackAttachment {
        FeedbackAttachment {
            filename: filename.to_string(),
            content_type: Some(content_type.to_string()),
            buffer: buffer.to_vec(),
        }
    }

    fn valid_attachments() -> Vec<FeedbackAttachment> {
        vec![attachment(
            "rollout-debug.jsonl.gz",
            "application/gzip",
            b"\x1f\x8bdebug",
        )]
    }

    fn upload(
        dsn: &str,
        timeout: Duration,
        note: Option<&str>,
        attachments: &[FeedbackAttachment],
    ) -> anyhow::Result<String> {
        upload_spine_feedback_with_config(
            SpineFeedbackUpload { note, attachments },
            SpineFeedbackTransportConfig {
                dsn,
                timeout,
                disable_proxy: true,
            },
        )
    }

    fn spawn_server(
        status: u16,
        response_delay: Duration,
    ) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept test request");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 8192];
            let header_end = loop {
                let read = stream.read(&mut chunk).expect("read test request");
                if read == 0 {
                    break request.len();
                }
                request.extend_from_slice(&chunk[..read]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let content_length = request[..header_end]
                .split(|byte| *byte == b'\n')
                .find_map(|line| {
                    let line = line.strip_suffix(b"\r")?;
                    let colon = line.iter().position(|byte| *byte == b':')?;
                    let (name, value) = line.split_at(colon);
                    let value = value.get(1..)?;
                    if name.eq_ignore_ascii_case(b"content-length") {
                        std::str::from_utf8(value).ok()?.trim().parse().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let read = stream.read(&mut chunk).expect("read test body");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            if !response_delay.is_zero() {
                thread::sleep(response_delay);
            }
            let response =
                format!("HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(response.as_bytes());
            request
        });
        (format!("{TEST_DSN_PREFIX}{address}/42"), handle)
    }

    #[test]
    fn accepts_2xx_and_returns_stable_event_id() {
        let (dsn, server) = spawn_server(202, Duration::ZERO);
        let attachments = valid_attachments();
        let report_id = upload(&dsn, Duration::from_secs(1), Some("a note"), &attachments)
            .expect("2xx should succeed");
        let request = server.join().expect("server should finish");
        let request_text = String::from_utf8_lossy(&request).to_ascii_lowercase();
        assert!(request_text.contains("x-sentry-auth: sentry sentry_key=public-key"));
        assert!(request_text.contains("content-type: application/x-sentry-envelope"));
        let body = request_body(&request);
        let (envelope_header, event) = captured_event(body);
        assert_eq!(
            envelope_header
                .get("event_id")
                .and_then(Value::as_str)
                .map(|value| value.replace('-', ""))
                .as_deref(),
            Some(report_id.as_str())
        );
        assert_eq!(
            event.get("event_id").and_then(Value::as_str),
            Some(report_id.as_str())
        );
        assert_eq!(report_id.len(), 32);
    }

    #[test]
    fn returns_failure_for_non_2xx_statuses() {
        for status in [400, 429, 500] {
            let (dsn, server) = spawn_server(status, Duration::ZERO);
            let attachments = valid_attachments();
            let result = upload(&dsn, Duration::from_secs(1), None, &attachments);
            let _request = server.join().expect("server should finish");
            assert!(result.is_err(), "status {status} must fail");
        }
    }

    #[test]
    fn returns_failure_for_timeout_and_connection_error() {
        let (dsn, server) = spawn_server(202, Duration::from_millis(250));
        let attachments = valid_attachments();
        let timeout_result = upload(&dsn, Duration::from_millis(50), None, &attachments);
        assert!(timeout_result.is_err());
        let _request = server.join().expect("server should finish");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind unused port");
        let address = listener.local_addr().expect("unused port address");
        drop(listener);
        let connection_result = upload(
            &format!("{TEST_DSN_PREFIX}{address}/42"),
            Duration::from_millis(100),
            None,
            &attachments,
        );
        assert!(connection_result.is_err());
    }

    #[test]
    fn rejects_oversized_or_unapproved_attachments() {
        let oversized = attachment(
            "rollout-debug.jsonl.gz",
            "application/gzip",
            &vec![0; 20 * 1024 * 1024 + 1],
        );
        let result = upload(
            "http://public-key@127.0.0.1:1/42",
            Duration::from_millis(50),
            None,
            &[oversized],
        );
        assert!(result.is_err());

        let arbitrary = attachment("thread-raw.log", "text/plain", b"raw");
        let result = upload(
            "http://public-key@127.0.0.1:1/42",
            Duration::from_millis(50),
            None,
            &[arbitrary],
        );
        assert!(result.is_err());

        let missing_rollout = attachment("screenshot-1.png", "image/png", b"png");
        let result = upload(
            "http://public-key@127.0.0.1:1/42",
            Duration::from_millis(50),
            None,
            &[missing_rollout],
        );
        assert!(result.is_err());

        let duplicate_rollout = [
            attachment(
                "rollout-debug.jsonl.gz",
                "application/gzip",
                b"\x1f\x8bdebug",
            ),
            attachment(
                "rollout-debug.jsonl.gz",
                "application/gzip",
                b"\x1f\x8bdebug",
            ),
        ];
        let result = upload(
            "http://public-key@127.0.0.1:1/42",
            Duration::from_millis(50),
            None,
            &duplicate_rollout,
        );
        assert!(result.is_err());
    }

    #[test]
    fn accepts_exact_attachment_byte_limit() {
        let attachments = [attachment(
            "rollout-debug.jsonl.gz",
            "application/gzip",
            &vec![0; super::SPINE_FEEDBACK_MAX_ATTACHMENT_BYTES],
        )];

        super::validate_attachments(&attachments)
            .expect("the documented attachment byte limit is inclusive");
    }

    #[test]
    fn rejects_note_over_byte_limit() {
        let attachments = valid_attachments();
        let note = "x".repeat(super::SPINE_FEEDBACK_MAX_NOTE_BYTES + 1);
        let result = upload(
            "http://public-key@127.0.0.1:1/42",
            Duration::from_millis(50),
            Some(&note),
            &attachments,
        );
        assert!(result.is_err());
    }

    #[test]
    fn request_contains_no_thread_or_account_identifier() {
        let (dsn, server) = spawn_server(200, Duration::ZERO);
        let attachments = valid_attachments();
        let result = upload(&dsn, Duration::from_secs(1), Some("feedback"), &attachments)
            .expect("2xx should succeed");
        let request = server.join().expect("server should finish");
        let request = String::from_utf8_lossy(&request);
        assert!(request.contains("rollout-debug.jsonl.gz"));
        assert!(request.contains("feedback"));
        assert!(!request.contains("thread-raw"));
        assert!(!request.contains("account-raw"));
        assert_eq!(result.len(), 32);
    }

    #[test]
    fn optional_note_can_be_absent() {
        let (dsn, server) = spawn_server(200, Duration::ZERO);
        let attachments = valid_attachments();
        let result = upload(&dsn, Duration::from_secs(1), None, &attachments);
        let request = server.join().expect("server should finish");
        assert!(result.is_ok());
        let (_envelope_header, event) = captured_event(request_body(&request));
        assert!(event.get("extra").is_none());
    }

    fn request_body(request: &[u8]) -> &[u8] {
        let offset = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("request header terminator")
            + 4;
        &request[offset..]
    }

    fn captured_event(body: &[u8]) -> (Value, Value) {
        let mut lines = body.splitn(3, |byte| *byte == b'\n');
        let envelope_header =
            serde_json::from_slice(lines.next().expect("envelope header")).expect("header JSON");
        let item_header: Value =
            serde_json::from_slice(lines.next().expect("event item header")).expect("item JSON");
        assert_eq!(
            item_header.get("type").and_then(Value::as_str),
            Some("event")
        );
        let length = item_header
            .get("length")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .expect("event item length");
        let remainder = lines.next().expect("event item payload");
        let event =
            serde_json::from_slice(&remainder[..length]).expect("captured event payload JSON");
        (envelope_header, event)
    }
}
