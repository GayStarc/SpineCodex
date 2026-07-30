use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Cursor;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::SpineFeedbackScreenshot;
use codex_app_server_protocol::SpineFeedbackUploadParams;
use codex_app_server_protocol::SpineFeedbackUploadResponse;
use codex_core::RolloutDebugRedactor;
use codex_core::StateDbHandle;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_features::Feature;
use codex_feedback::FeedbackAttachment;
use codex_feedback::SPINE_FEEDBACK_MAX_ATTACHMENT_BYTES;
use codex_feedback::SPINE_FEEDBACK_MAX_NOTE_BYTES;
use codex_feedback::SPINE_ROLLOUT_DEBUG_ATTACHMENT_FILENAME;
use codex_feedback::SpineFeedbackUpload;
use codex_feedback::upload_spine_feedback;
use codex_protocol::ThreadId;
use flate2::Compression;
use flate2::GzBuilder;
use image::ImageFormat;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::error_code::internal_error;
use crate::error_code::invalid_request;

const ROLLOUT_DEBUG_SCHEMA: &str = "spine.rollout_debug.v1";
const ROLLOUT_DEBUG_CONTENT_TYPE: &str = "application/gzip";
const SCREENSHOT_CONTENT_TYPE: &str = "image/png";
const SCREENSHOT_FILENAMES: [&str; 3] =
    ["screenshot-1.png", "screenshot-2.png", "screenshot-3.png"];
const MAX_SCREENSHOTS: usize = SCREENSHOT_FILENAMES.len();
const MAX_SCREENSHOT_BYTES: usize = 5 * 1024 * 1024;
const MAX_SCREENSHOT_TOTAL_BYTES: usize = 10 * 1024 * 1024;
const MAX_SCREENSHOT_SIDE: u32 = 8192;
const MAX_SCREENSHOT_PIXELS: u64 = 16_000_000;
const MAX_SCREENSHOT_BASE64_BYTES: usize = ((MAX_SCREENSHOT_BYTES + 2) / 3) * 4 + 4;
const MAX_SOURCE_LINE_BYTES: usize = 8 * 1024 * 1024;
const ROLLOUT_READER_CAPACITY: usize = 64 * 1024;

pub(super) async fn spine_feedback_upload(
    thread_manager: Arc<ThreadManager>,
    config: Arc<Config>,
    state_db: Option<StateDbHandle>,
    params: SpineFeedbackUploadParams,
) -> Result<SpineFeedbackUploadResponse, JSONRPCErrorError> {
    if !config.feedback_enabled {
        return Err(invalid_request(
            "sending feedback is disabled by configuration",
        ));
    }

    let SpineFeedbackUploadParams {
        thread_id,
        note,
        screenshots,
    } = params;
    if note
        .as_ref()
        .is_some_and(|note| note.len() > SPINE_FEEDBACK_MAX_NOTE_BYTES)
    {
        return Err(invalid_request(format!(
            "Spine feedback note exceeds {SPINE_FEEDBACK_MAX_NOTE_BYTES} bytes"
        )));
    }

    let root_thread_id = ThreadId::from_string(&thread_id)
        .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;
    let root_thread = thread_manager
        .get_thread(root_thread_id)
        .await
        .map_err(|_| invalid_request("Spine feedback requires an active thread"))?;
    if !spine_feedback_enabled(&root_thread) {
        return Err(invalid_request(
            "feedback/spineUpload requires a Spine-enabled thread",
        ));
    }

    let screenshots = tokio::task::spawn_blocking(move || normalize_screenshots(screenshots))
        .await
        .map_err(|err| internal_error(format!("failed to validate screenshots: {err}")))?
        .map_err(invalid_request)?;
    let screenshot_bytes = screenshots
        .iter()
        .map(|attachment| attachment.buffer.len())
        .sum::<usize>();

    let subtree_thread_ids = thread_manager
        .list_agent_subtree_thread_ids(root_thread_id)
        .await
        .map_err(|err| internal_error(format!("failed to snapshot Spine thread subtree: {err}")))?;
    let subtree_thread_ids = normalize_subtree_thread_ids(root_thread_id, subtree_thread_ids);
    let parent_thread_ids =
        resolve_parent_thread_ids(&thread_manager, state_db.as_ref(), &subtree_thread_ids).await;
    let captures =
        capture_rollout_sources(&thread_manager, state_db.as_ref(), &subtree_thread_ids).await;

    let rollout_limit = SPINE_FEEDBACK_MAX_ATTACHMENT_BYTES
        .checked_sub(screenshot_bytes)
        .ok_or_else(|| invalid_request("Spine feedback screenshots exceed the attachment limit"))?;
    let rollout_bytes = tokio::task::spawn_blocking(move || {
        build_rollout_debug_attachment(
            root_thread_id,
            captures,
            parent_thread_ids,
            rollout_limit,
            MAX_SOURCE_LINE_BYTES,
        )
    })
    .await
    .map_err(|err| internal_error(format!("failed to build rollout debug attachment: {err}")))?
    .map_err(map_bundle_error)?;

    let mut attachments = Vec::with_capacity(screenshots.len() + 1);
    attachments.push(FeedbackAttachment {
        filename: SPINE_ROLLOUT_DEBUG_ATTACHMENT_FILENAME.to_string(),
        content_type: Some(ROLLOUT_DEBUG_CONTENT_TYPE.to_string()),
        buffer: rollout_bytes,
    });
    attachments.extend(screenshots);

    let upload_result = tokio::task::spawn_blocking(move || {
        upload_spine_feedback(SpineFeedbackUpload {
            note: note.as_deref(),
            attachments: &attachments,
        })
    })
    .await
    .map_err(|err| internal_error(format!("failed to upload Spine feedback: {err}")))?;

    upload_result_to_response(upload_result)
}

fn spine_feedback_enabled(thread: &codex_core::CodexThread) -> bool {
    spine_feedback_enabled_by(|feature| thread.enabled(feature))
}

fn spine_feedback_enabled_by(mut enabled: impl FnMut(Feature) -> bool) -> bool {
    [Feature::SpineJit, Feature::SpineTrim, Feature::SpineSpawn]
        .into_iter()
        .any(&mut enabled)
}

fn normalize_subtree_thread_ids(
    root_thread_id: ThreadId,
    thread_ids: Vec<ThreadId>,
) -> Vec<ThreadId> {
    let mut seen = HashSet::new();
    seen.insert(root_thread_id);
    let mut descendants = thread_ids
        .into_iter()
        .filter(|thread_id| *thread_id != root_thread_id && seen.insert(*thread_id))
        .collect::<Vec<_>>();
    descendants.sort_unstable_by_key(ToString::to_string);

    let mut normalized = Vec::with_capacity(descendants.len() + 1);
    normalized.push(root_thread_id);
    normalized.extend(descendants);
    normalized
}

async fn resolve_parent_thread_ids(
    thread_manager: &ThreadManager,
    state_db: Option<&StateDbHandle>,
    thread_ids: &[ThreadId],
) -> HashMap<ThreadId, ThreadId> {
    let thread_id_set = thread_ids.iter().copied().collect::<HashSet<_>>();
    let mut parents = HashMap::new();

    for thread_id in thread_ids {
        if let Ok(thread) = thread_manager.get_thread(*thread_id).await
            && let Some(parent_thread_id) = thread.config_snapshot().await.parent_thread_id
        {
            parents.insert(*thread_id, parent_thread_id);
        }
    }

    if let Some(state_db) = state_db {
        for parent_thread_id in thread_ids {
            let Ok(child_thread_ids) = state_db.list_thread_spawn_children(*parent_thread_id).await
            else {
                continue;
            };
            for child_thread_id in child_thread_ids {
                if thread_id_set.contains(&child_thread_id) {
                    parents.entry(child_thread_id).or_insert(*parent_thread_id);
                }
            }
        }
    }

    parents
}

async fn capture_rollout_sources(
    thread_manager: &ThreadManager,
    state_db: Option<&StateDbHandle>,
    thread_ids: &[ThreadId],
) -> Vec<CapturedThread> {
    let mut captures = Vec::with_capacity(thread_ids.len());
    for thread_id in thread_ids {
        let source = match thread_manager.get_thread(*thread_id).await {
            Ok(thread) => {
                if thread.flush_rollout().await.is_err() {
                    CapturedSource::FlushFailed
                } else if let Some(path) = thread.rollout_path() {
                    capture_path(path).await
                } else {
                    CapturedSource::Missing
                }
            }
            Err(_) => match state_db {
                Some(state_db) => match state_db
                    .find_rollout_path_by_id(*thread_id, /*archived_only*/ None)
                    .await
                {
                    Ok(Some(path)) => capture_path(path).await,
                    Ok(None) => CapturedSource::Missing,
                    Err(_) => CapturedSource::Unavailable,
                },
                None => CapturedSource::Unavailable,
            },
        };
        captures.push(CapturedThread {
            thread_id: *thread_id,
            source,
        });
    }
    captures
}

async fn capture_path(path: PathBuf) -> CapturedSource {
    match tokio::fs::metadata(&path).await {
        Ok(metadata) if metadata.is_file() => CapturedSource::Ready {
            path,
            captured_bytes: metadata.len(),
        },
        Ok(_) => CapturedSource::Unreadable,
        Err(err) if err.kind() == io::ErrorKind::NotFound => CapturedSource::Missing,
        Err(_) => CapturedSource::Unreadable,
    }
}

#[derive(Debug)]
struct CapturedThread {
    thread_id: ThreadId,
    source: CapturedSource,
}

#[derive(Debug)]
enum CapturedSource {
    Ready { path: PathBuf, captured_bytes: u64 },
    Missing,
    FlushFailed,
    Unavailable,
    Unreadable,
}

#[derive(Serialize)]
struct RolloutDebugManifest {
    record_type: &'static str,
    schema: &'static str,
    build: &'static str,
    root_thread_local_id: u64,
    thread_count: usize,
    threads: Vec<ManifestThread>,
}

#[derive(Serialize)]
struct ManifestThread {
    thread_local_id: u64,
    parent: ManifestParent,
    source: ManifestSource,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum ManifestParent {
    Root,
    Known { thread_local_id: u64 },
    OutsideSnapshot,
    Unknown,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum ManifestSource {
    Ready { captured_bytes: u64 },
    Missing,
    FlushFailed,
    Unavailable,
    Unreadable,
}

#[derive(Serialize)]
struct RolloutDebugThreadRecord {
    record_type: &'static str,
    thread_local_id: u64,
    ordinal: u64,
    item: Value,
}

fn build_rollout_debug_attachment(
    root_thread_id: ThreadId,
    mut captures: Vec<CapturedThread>,
    parent_thread_ids: HashMap<ThreadId, ThreadId>,
    output_limit: usize,
    source_line_limit: usize,
) -> Result<Vec<u8>, BundleBuildError> {
    preflight_rollout_sources(&mut captures);

    let mut redactor = RolloutDebugRedactor::default();
    let local_thread_ids = captures
        .iter()
        .map(|capture| {
            (
                capture.thread_id,
                redactor.register_thread_id(&capture.thread_id.to_string()),
            )
        })
        .collect::<HashMap<_, _>>();
    let root_thread_local_id = local_thread_ids[&root_thread_id];
    let manifest_threads = captures
        .iter()
        .map(|capture| ManifestThread {
            thread_local_id: local_thread_ids[&capture.thread_id],
            parent: manifest_parent(
                capture.thread_id,
                root_thread_id,
                &parent_thread_ids,
                &local_thread_ids,
            ),
            source: manifest_source(&capture.source),
        })
        .collect::<Vec<_>>();
    let manifest = RolloutDebugManifest {
        record_type: "manifest",
        schema: ROLLOUT_DEBUG_SCHEMA,
        build: env!("CARGO_PKG_VERSION"),
        root_thread_local_id,
        thread_count: manifest_threads.len(),
        threads: manifest_threads,
    };

    let exceeded = Arc::new(AtomicBool::new(false));
    let capped = CappedWriter::new(output_limit, Arc::clone(&exceeded));
    let mut gzip = GzBuilder::new()
        .mtime(0)
        .write(capped, Compression::default());
    write_json_line(&mut gzip, &manifest, output_limit, &exceeded)?;

    for capture in captures {
        let CapturedSource::Ready {
            path,
            captured_bytes,
        } = capture.source
        else {
            continue;
        };
        let file = File::open(path).map_err(BundleBuildError::SourceRead)?;
        let mut reader = BufReader::with_capacity(ROLLOUT_READER_CAPACITY, file);
        let mut remaining = captured_bytes;
        let mut ordinal = 0_u64;
        while let Some(line) =
            read_bounded_source_line(&mut reader, &mut remaining, source_line_limit)
                .map_err(BundleBuildError::SourceRead)?
        {
            let item = match line {
                BoundedSourceLine::Retained(line) => {
                    redactor.redact_json_line_to_value(line.as_slice())
                }
                BoundedSourceLine::Oversized => RolloutDebugRedactor::oversized_value(),
            };
            let record = RolloutDebugThreadRecord {
                record_type: "thread_record",
                thread_local_id: local_thread_ids[&capture.thread_id],
                ordinal,
                item,
            };
            write_json_line(&mut gzip, &record, output_limit, &exceeded)?;
            ordinal = ordinal.saturating_add(1);
        }
        if remaining != 0 {
            return Err(BundleBuildError::SourceRead(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "rollout source changed before its captured boundary",
            )));
        }
    }

    let capped = gzip.finish().map_err(|err| {
        if exceeded.load(Ordering::Relaxed) {
            BundleBuildError::AttachmentTooLarge {
                limit: output_limit,
            }
        } else {
            BundleBuildError::Encoding(err)
        }
    })?;
    Ok(capped.into_inner())
}

fn preflight_rollout_sources(captures: &mut [CapturedThread]) {
    for capture in captures {
        let replacement = match &capture.source {
            CapturedSource::Ready { path, .. } => match File::open(path) {
                Ok(_) => None,
                Err(err) if err.kind() == io::ErrorKind::NotFound => Some(CapturedSource::Missing),
                Err(_) => Some(CapturedSource::Unreadable),
            },
            CapturedSource::Missing
            | CapturedSource::FlushFailed
            | CapturedSource::Unavailable
            | CapturedSource::Unreadable => None,
        };
        if let Some(replacement) = replacement {
            capture.source = replacement;
        }
    }
}

fn manifest_parent(
    thread_id: ThreadId,
    root_thread_id: ThreadId,
    parent_thread_ids: &HashMap<ThreadId, ThreadId>,
    local_thread_ids: &HashMap<ThreadId, u64>,
) -> ManifestParent {
    if thread_id == root_thread_id {
        return ManifestParent::Root;
    }
    let Some(parent_thread_id) = parent_thread_ids.get(&thread_id) else {
        return ManifestParent::Unknown;
    };
    match local_thread_ids.get(parent_thread_id) {
        Some(thread_local_id) => ManifestParent::Known {
            thread_local_id: *thread_local_id,
        },
        None => ManifestParent::OutsideSnapshot,
    }
}

fn manifest_source(source: &CapturedSource) -> ManifestSource {
    match source {
        CapturedSource::Ready { captured_bytes, .. } => ManifestSource::Ready {
            captured_bytes: *captured_bytes,
        },
        CapturedSource::Missing => ManifestSource::Missing,
        CapturedSource::FlushFailed => ManifestSource::FlushFailed,
        CapturedSource::Unavailable => ManifestSource::Unavailable,
        CapturedSource::Unreadable => ManifestSource::Unreadable,
    }
}

fn write_json_line<T: Serialize>(
    writer: &mut impl Write,
    value: &T,
    output_limit: usize,
    exceeded: &AtomicBool,
) -> Result<(), BundleBuildError> {
    if let Err(err) = serde_json::to_writer(&mut *writer, value) {
        if exceeded.load(Ordering::Relaxed) {
            return Err(BundleBuildError::AttachmentTooLarge {
                limit: output_limit,
            });
        }
        return Err(BundleBuildError::Serialization(err));
    }
    if let Err(err) = writer.write_all(b"\n") {
        if exceeded.load(Ordering::Relaxed) {
            return Err(BundleBuildError::AttachmentTooLarge {
                limit: output_limit,
            });
        }
        return Err(BundleBuildError::Encoding(err));
    }
    Ok(())
}

enum BoundedSourceLine {
    Retained(Vec<u8>),
    Oversized,
}

fn read_bounded_source_line<R: BufRead>(
    reader: &mut R,
    remaining: &mut u64,
    retained_limit: usize,
) -> io::Result<Option<BoundedSourceLine>> {
    if *remaining == 0 {
        return Ok(None);
    }

    let mut retained = Vec::new();
    let mut oversized = false;
    let mut saw_bytes = false;
    loop {
        let (consume_len, line_complete) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return if saw_bytes {
                    Ok(Some(if oversized {
                        BoundedSourceLine::Oversized
                    } else {
                        BoundedSourceLine::Retained(retained)
                    }))
                } else {
                    Ok(None)
                };
            }
            let available_len = usize::try_from(
                (*remaining).min(u64::try_from(available.len()).unwrap_or(u64::MAX)),
            )
            .unwrap_or(available.len());
            let bounded = &available[..available_len];
            let newline = bounded.iter().position(|byte| *byte == b'\n');
            let consume_len = newline.map_or(available_len, |index| index + 1);
            saw_bytes = saw_bytes || consume_len != 0;
            if !oversized {
                if retained.len().saturating_add(consume_len) > retained_limit {
                    retained.clear();
                    oversized = true;
                } else {
                    retained.extend_from_slice(&bounded[..consume_len]);
                }
            }
            (consume_len, newline.is_some())
        };

        reader.consume(consume_len);
        *remaining = remaining.saturating_sub(u64::try_from(consume_len).unwrap_or(u64::MAX));
        if line_complete || *remaining == 0 {
            return Ok(Some(if oversized {
                BoundedSourceLine::Oversized
            } else {
                BoundedSourceLine::Retained(retained)
            }));
        }
    }
}

struct CappedWriter {
    buffer: Vec<u8>,
    limit: usize,
    exceeded: Arc<AtomicBool>,
}

impl CappedWriter {
    fn new(limit: usize, exceeded: Arc<AtomicBool>) -> Self {
        Self {
            buffer: Vec::with_capacity(limit.min(1024 * 1024)),
            limit,
            exceeded,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.buffer
    }
}

impl Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next_len) = self.buffer.len().checked_add(bytes.len()) else {
            self.exceeded.store(true, Ordering::Relaxed);
            return Err(io::Error::other("rollout debug attachment size overflow"));
        };
        if next_len > self.limit {
            self.exceeded.store(true, Ordering::Relaxed);
            return Err(io::Error::other("rollout debug attachment limit exceeded"));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Error)]
enum BundleBuildError {
    #[error("rollout debug attachment exceeds {limit} bytes")]
    AttachmentTooLarge { limit: usize },
    #[error("failed to read captured rollout source")]
    SourceRead(#[source] io::Error),
    #[error("failed to encode rollout debug attachment")]
    Encoding(#[source] io::Error),
    #[error("failed to serialize rollout debug record")]
    Serialization(#[source] serde_json::Error),
}

fn map_bundle_error(error: BundleBuildError) -> JSONRPCErrorError {
    match error {
        BundleBuildError::AttachmentTooLarge { .. } => invalid_request(error.to_string()),
        BundleBuildError::SourceRead(_)
        | BundleBuildError::Encoding(_)
        | BundleBuildError::Serialization(_) => internal_error(error.to_string()),
    }
}

fn normalize_screenshots(
    screenshots: Vec<SpineFeedbackScreenshot>,
) -> Result<Vec<FeedbackAttachment>, String> {
    if screenshots.len() > MAX_SCREENSHOTS {
        return Err(format!(
            "Spine feedback accepts at most {MAX_SCREENSHOTS} screenshots"
        ));
    }

    let mut attachments = Vec::with_capacity(screenshots.len());
    let mut total_bytes = 0_usize;
    for (index, screenshot) in screenshots.into_iter().enumerate() {
        if screenshot.png_base64.len() > MAX_SCREENSHOT_BASE64_BYTES {
            return Err(format!("screenshot {} is too large", index + 1));
        }
        let input = BASE64_STANDARD
            .decode(screenshot.png_base64.as_bytes())
            .map_err(|_| format!("screenshot {} is not valid base64", index + 1))?;
        if input.len() > MAX_SCREENSHOT_BYTES {
            return Err(format!("screenshot {} is too large", index + 1));
        }
        if image::guess_format(&input).ok() != Some(ImageFormat::Png) {
            return Err(format!("screenshot {} is not a PNG image", index + 1));
        }

        let dimensions =
            image::ImageReader::with_format(Cursor::new(input.as_slice()), ImageFormat::Png)
                .into_dimensions()
                .map_err(|_| format!("screenshot {} has invalid PNG dimensions", index + 1))?;
        validate_screenshot_dimensions(index, dimensions)?;
        let image = image::load_from_memory_with_format(&input, ImageFormat::Png)
            .map_err(|_| format!("screenshot {} is not a valid PNG image", index + 1))?;
        let mut normalized = Cursor::new(Vec::new());
        image
            .write_to(&mut normalized, ImageFormat::Png)
            .map_err(|_| format!("screenshot {} could not be normalized", index + 1))?;
        let normalized = normalized.into_inner();
        if normalized.len() > MAX_SCREENSHOT_BYTES {
            return Err(format!(
                "normalized screenshot {} exceeds {MAX_SCREENSHOT_BYTES} bytes",
                index + 1
            ));
        }
        total_bytes = total_bytes
            .checked_add(normalized.len())
            .ok_or_else(|| "screenshot byte count overflowed".to_string())?;
        if total_bytes > MAX_SCREENSHOT_TOTAL_BYTES {
            return Err(format!(
                "Spine feedback screenshots exceed {MAX_SCREENSHOT_TOTAL_BYTES} bytes"
            ));
        }

        attachments.push(FeedbackAttachment {
            filename: SCREENSHOT_FILENAMES[index].to_string(),
            content_type: Some(SCREENSHOT_CONTENT_TYPE.to_string()),
            buffer: normalized,
        });
    }
    Ok(attachments)
}

fn validate_screenshot_dimensions(index: usize, (width, height): (u32, u32)) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err(format!("screenshot {} has zero dimensions", index + 1));
    }
    if width > MAX_SCREENSHOT_SIDE || height > MAX_SCREENSHOT_SIDE {
        return Err(format!(
            "screenshot {} exceeds {MAX_SCREENSHOT_SIDE} pixels per side",
            index + 1
        ));
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_SCREENSHOT_PIXELS {
        return Err(format!(
            "screenshot {} exceeds {MAX_SCREENSHOT_PIXELS} pixels",
            index + 1
        ));
    }
    Ok(())
}

fn upload_result_to_response(
    upload_result: anyhow::Result<String>,
) -> Result<SpineFeedbackUploadResponse, JSONRPCErrorError> {
    upload_result
        .map(|report_id| SpineFeedbackUploadResponse { report_id })
        .map_err(|err| internal_error(format!("failed to upload Spine feedback: {err}")))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Read;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use flate2::read::GzDecoder;
    use image::DynamicImage;
    use image::ImageBuffer;
    use image::ImageFormat;
    use image::Rgba;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    const SECRET: &str = "private-spine-feedback-secret";

    fn thread_id(index: u8) -> ThreadId {
        ThreadId::from_string(&format!("01900000-0000-7000-8000-{index:012x}"))
            .expect("valid thread id")
    }

    fn write_source(tempdir: &TempDir, name: &str, bytes: &[u8]) -> CapturedSource {
        let path = tempdir.path().join(name);
        std::fs::write(&path, bytes).expect("write source");
        CapturedSource::Ready {
            path,
            captured_bytes: u64::try_from(bytes.len()).expect("source length fits"),
        }
    }

    fn decode_lines(bytes: &[u8]) -> Vec<Value> {
        let mut decoded = String::new();
        GzDecoder::new(bytes)
            .read_to_string(&mut decoded)
            .expect("decode gzip");
        decoded
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSONL"))
            .collect()
    }

    #[test]
    fn bundle_keeps_complete_topology_order_and_positional_placeholders() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = thread_id(0);
        let mut captures = Vec::new();
        let oversized = SECRET.repeat(64);
        let root_source = format!(
            "{{\"timestamp\":\"{SECRET}\",\"type\":\"future_secret\",\"payload\":{{\"value\":\"{SECRET}\"}}}}\n\
             {{\"timestamp\":\"{SECRET}\"}}\n\
             {oversized}\n"
        );
        captures.push(CapturedThread {
            thread_id: root,
            source: write_source(&tempdir, "root.jsonl", root_source.as_bytes()),
        });
        for index in 1..10 {
            captures.push(CapturedThread {
                thread_id: thread_id(index),
                source: CapturedSource::Missing,
            });
        }
        let parents = (1..10)
            .map(|index| {
                let parent = if index == 1 {
                    root
                } else {
                    thread_id(index - 1)
                };
                (thread_id(index), parent)
            })
            .collect::<HashMap<_, _>>();

        let first = build_rollout_debug_attachment(root, captures, parents, 1024 * 1024, 512)
            .expect("build attachment");
        let lines = decode_lines(&first);
        let decompressed = serde_json::to_string(&lines).expect("serialize lines");
        assert!(!decompressed.contains(SECRET));
        for index in 0..10 {
            assert!(!decompressed.contains(&thread_id(index).to_string()));
        }

        assert_eq!(lines[0]["record_type"], "manifest");
        assert_eq!(lines[0]["thread_count"], 10);
        assert_eq!(lines[0]["threads"][0]["parent"]["state"], "root");
        assert_eq!(lines[0]["threads"][9]["parent"]["state"], "known");
        assert_eq!(lines[1]["ordinal"], 0);
        assert_eq!(lines[1]["item"]["record_type"], "unknown_redacted");
        assert_eq!(lines[2]["ordinal"], 1);
        assert_eq!(lines[2]["item"]["record_type"], "malformed_redacted");
        assert_eq!(lines[3]["ordinal"], 2);
        assert_eq!(lines[3]["item"]["record_type"], "oversized_redacted");
    }

    #[test]
    fn bundle_is_deterministic_and_enforces_compressed_limit() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = thread_id(0);
        let source = br#"{"timestamp":"x","type":"future","payload":{}}"#;
        let make_captures = || {
            vec![CapturedThread {
                thread_id: root,
                source: write_source(&tempdir, "deterministic.jsonl", source),
            }]
        };

        let first =
            build_rollout_debug_attachment(root, make_captures(), HashMap::new(), 4096, 4096)
                .expect("first build");
        let second =
            build_rollout_debug_attachment(root, make_captures(), HashMap::new(), 4096, 4096)
                .expect("second build");
        assert_eq!(first, second);

        let error = build_rollout_debug_attachment(root, make_captures(), HashMap::new(), 1, 4096)
            .expect_err("tiny output cap must fail");
        assert!(matches!(
            error,
            BundleBuildError::AttachmentTooLarge { limit: 1 }
        ));
    }

    #[test]
    fn source_states_and_parent_boundaries_are_explicit() {
        let root = thread_id(0);
        let child = thread_id(1);
        let outside = thread_id(2);
        let local_ids = HashMap::from([(root, 0), (child, 1)]);

        assert!(matches!(
            manifest_parent(root, root, &HashMap::new(), &local_ids),
            ManifestParent::Root
        ));
        assert!(matches!(
            manifest_parent(child, root, &HashMap::from([(child, root)]), &local_ids),
            ManifestParent::Known { thread_local_id: 0 }
        ));
        assert!(matches!(
            manifest_parent(child, root, &HashMap::from([(child, outside)]), &local_ids),
            ManifestParent::OutsideSnapshot
        ));
        assert!(matches!(
            manifest_parent(child, root, &HashMap::new(), &local_ids),
            ManifestParent::Unknown
        ));
        for source in [
            CapturedSource::Missing,
            CapturedSource::FlushFailed,
            CapturedSource::Unavailable,
            CapturedSource::Unreadable,
        ] {
            let serialized =
                serde_json::to_value(manifest_source(&source)).expect("source serializes");
            assert!(serialized.get("state").is_some());
        }
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = ImageBuffer::from_pixel(width, height, Rgba([1, 2, 3, 255]));
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("encode PNG");
        bytes.into_inner()
    }

    #[test]
    fn screenshots_are_reencoded_with_fixed_names_and_no_trailing_metadata() {
        let mut input = png(2, 2);
        input.extend_from_slice(SECRET.as_bytes());
        let screenshots = vec![SpineFeedbackScreenshot {
            png_base64: BASE64_STANDARD.encode(input),
        }];

        let attachments = normalize_screenshots(screenshots).expect("normalize screenshot");
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "screenshot-1.png");
        assert_eq!(
            attachments[0].content_type.as_deref(),
            Some(SCREENSHOT_CONTENT_TYPE)
        );
        assert!(
            !attachments[0]
                .buffer
                .windows(SECRET.len())
                .any(|window| window == SECRET.as_bytes())
        );
        assert_eq!(
            image::guess_format(&attachments[0].buffer).expect("guess PNG"),
            ImageFormat::Png
        );
    }

    #[test]
    fn screenshot_validation_is_fail_closed() {
        let too_many = vec![
            SpineFeedbackScreenshot {
                png_base64: BASE64_STANDARD.encode(png(1, 1)),
            };
            MAX_SCREENSHOTS + 1
        ];
        assert!(normalize_screenshots(too_many).is_err());
        assert!(
            normalize_screenshots(vec![SpineFeedbackScreenshot {
                png_base64: "not-base64".to_string(),
            }])
            .is_err()
        );
        assert!(
            normalize_screenshots(vec![SpineFeedbackScreenshot {
                png_base64: BASE64_STANDARD.encode(b"not-a-png"),
            }])
            .is_err()
        );
        assert!(validate_screenshot_dimensions(0, (0, 1)).is_err());
        assert!(validate_screenshot_dimensions(0, (MAX_SCREENSHOT_SIDE + 1, 1)).is_err());
        assert!(validate_screenshot_dimensions(0, (4001, 4000)).is_err());
        assert!(validate_screenshot_dimensions(0, (4000, 4000)).is_ok());
    }

    #[test]
    fn upload_result_propagates_report_id_and_transport_failure() {
        let response =
            upload_result_to_response(Ok("0123456789abcdef0123456789abcdef".to_string()))
                .expect("success response");
        assert_eq!(response.report_id, "0123456789abcdef0123456789abcdef");

        let error = upload_result_to_response(Err(anyhow::anyhow!("HTTP 429")))
            .expect_err("transport failure must propagate");
        assert_eq!(error.code, crate::error_code::INTERNAL_ERROR_CODE);
        assert!(error.message.contains("HTTP 429"));
    }

    #[test]
    fn normalize_subtree_is_root_first_deduplicated_and_deterministic() {
        let root = thread_id(0);
        let normalized = normalize_subtree_thread_ids(
            root,
            vec![thread_id(3), root, thread_id(1), thread_id(3), thread_id(2)],
        );
        assert_eq!(
            normalized,
            vec![root, thread_id(1), thread_id(2), thread_id(3)]
        );
    }

    #[test]
    fn feature_off_is_rejected_and_each_spine_authority_is_accepted() {
        assert!(!spine_feedback_enabled_by(|_| false));
        for enabled_feature in [Feature::SpineJit, Feature::SpineTrim, Feature::SpineSpawn] {
            assert!(spine_feedback_enabled_by(|feature| {
                feature == enabled_feature
            }));
        }
    }

    #[test]
    fn redactor_facade_preserves_package_local_thread_equality() {
        let mut redactor = RolloutDebugRedactor::default();
        let first = redactor.register_thread_id("raw-thread-a");
        let second = redactor.register_thread_id("raw-thread-b");
        assert_eq!(first, redactor.register_thread_id("raw-thread-a"));
        assert_ne!(first, second);
    }

    #[test]
    fn protocol_note_is_optional_and_screenshots_default_empty() {
        let params: SpineFeedbackUploadParams = serde_json::from_value(json!({
            "threadId": thread_id(0).to_string()
        }))
        .expect("deserialize params");
        assert_eq!(params.note, None);
        assert!(params.screenshots.is_empty());
    }
}
