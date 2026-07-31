use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::OnceLock;

use crate::app_command::AppCommand;
use crate::legacy_core::config::Config;
use serde::Serialize;
use serde_json::json;

use crate::app_event::AppEvent;

static LOGGER: LazyLock<SessionLogger> = LazyLock::new(SessionLogger::new);

struct SessionLogger {
    file: OnceLock<Mutex<File>>,
}

impl SessionLogger {
    fn new() -> Self {
        Self {
            file: OnceLock::new(),
        }
    }

    fn open(&self, path: PathBuf) -> std::io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }

        let file = opts.open(path)?;
        self.file.get_or_init(|| Mutex::new(file));
        Ok(())
    }

    fn write_json_line(&self, value: serde_json::Value) {
        let Some(mutex) = self.file.get() else {
            return;
        };
        let mut guard = match mutex.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match serde_json::to_string(&value) {
            Ok(serialized) => {
                if let Err(e) = guard.write_all(serialized.as_bytes()) {
                    tracing::warn!("session log write error: {}", e);
                    return;
                }
                if let Err(e) = guard.write_all(b"\n") {
                    tracing::warn!("session log write error: {}", e);
                    return;
                }
                if let Err(e) = guard.flush() {
                    tracing::warn!("session log flush error: {}", e);
                }
            }
            Err(e) => tracing::warn!("session log serialize error: {}", e),
        }
    }

    fn is_enabled(&self) -> bool {
        self.file.get().is_some()
    }
}

fn now_ts() -> String {
    // RFC3339 for readability; consumers can parse as needed.
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) fn maybe_init(config: &Config) {
    let enabled = std::env::var("CODEX_TUI_RECORD_SESSION")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);
    if !enabled {
        return;
    }

    let path = if let Ok(path) = std::env::var("CODEX_TUI_SESSION_LOG_PATH") {
        PathBuf::from(path)
    } else {
        let mut p = config.log_dir.clone();
        let filename = format!(
            "session-{}.jsonl",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        );
        p.push(filename);
        p
    };

    if let Err(e) = LOGGER.open(path.clone()) {
        tracing::error!("failed to open session log {:?}: {}", path, e);
        return;
    }

    // Write a header record so we can attach context.
    let header = json!({
        "ts": now_ts(),
        "dir": "meta",
        "kind": "session_start",
        "cwd": config.cwd,
        "model": config.model,
        "model_provider_id": config.model_provider_id,
        "model_provider_name": config.model_provider.name,
    });
    LOGGER.write_json_line(header);
}

pub(crate) fn log_inbound_app_event(event: &AppEvent) {
    // Log only if enabled
    if !LOGGER.is_enabled() {
        return;
    }
    log_inbound_app_event_to(&LOGGER, event);
}

fn log_inbound_app_event_to(logger: &SessionLogger, event: &AppEvent) {
    match event {
        AppEvent::NewSession => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "new_session",
            });
            logger.write_json_line(value);
        }
        AppEvent::ClearUi => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "clear_ui",
            });
            logger.write_json_line(value);
        }
        AppEvent::InsertHistoryCell(cell) => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "insert_history_cell",
                "lines": cell.transcript_lines(u16::MAX).len(),
            });
            logger.write_json_line(value);
        }
        AppEvent::StartFileSearch(query) => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "file_search_start",
                "query": query,
            });
            logger.write_json_line(value);
        }
        AppEvent::FileSearchResult { query, matches } => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "file_search_result",
                "query": query,
                "matches": matches.len(),
            });
            logger.write_json_line(value);
        }
        AppEvent::PetPreviewLoaded { request_id, result } => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": "PetPreviewLoaded",
                "request_id": request_id,
                "ok": result.is_ok(),
            });
            logger.write_json_line(value);
        }
        AppEvent::PetSelectionLoaded {
            request_id,
            pet_id,
            result,
        } => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": "PetSelectionLoaded",
                "request_id": request_id,
                "pet_id": pet_id,
                "ok": result.is_ok(),
            });
            logger.write_json_line(value);
        }
        AppEvent::SubmitSpineFeedback { .. } => {
            logger.write_json_line(json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": "SubmitSpineFeedback",
            }));
        }
        AppEvent::SpineFeedbackSubmitted { result, .. } => {
            logger.write_json_line(json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": "SpineFeedbackSubmitted",
                "ok": result.is_ok(),
            }));
        }
        // Noise or control flow – record variant only
        other => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": format!("{other:?}").split('(').next().unwrap_or("app_event"),
            });
            logger.write_json_line(value);
        }
    }
}

pub(crate) fn log_outbound_op(op: &AppCommand) {
    if !LOGGER.is_enabled() {
        return;
    }
    write_record("from_tui", "op", op);
}

pub(crate) fn log_session_end() {
    if !LOGGER.is_enabled() {
        return;
    }
    let value = json!({
        "ts": now_ts(),
        "dir": "meta",
        "kind": "session_end",
    });
    LOGGER.write_json_line(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bottom_pane::SpineFeedbackDraft;
    use crate::clipboard_paste::PreparedFeedbackScreenshot;
    use codex_protocol::ThreadId;

    #[test]
    fn spine_feedback_events_do_not_persist_draft_contents() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        let logger = SessionLogger::new();
        logger.open(path.clone()).expect("open session log");

        let thread_id = ThreadId::new();
        let note = "private-note-canary";
        let png = vec![17, 34, 51, 68];
        let draft = SpineFeedbackDraft {
            thread_id,
            note: note.to_string(),
            screenshots: vec![PreparedFeedbackScreenshot {
                png: png.clone(),
                width: 1,
                height: 1,
            }],
        };
        log_inbound_app_event_to(
            &logger,
            &AppEvent::SubmitSpineFeedback {
                draft: draft.clone(),
            },
        );
        log_inbound_app_event_to(
            &logger,
            &AppEvent::SpineFeedbackSubmitted {
                request_generation: 1,
                draft,
                result: Ok("report-id".to_string()),
            },
        );

        let logged = std::fs::read_to_string(path).expect("read session log");
        assert!(logged.contains("\"variant\":\"SubmitSpineFeedback\""));
        assert!(logged.contains("\"variant\":\"SpineFeedbackSubmitted\""));
        assert!(logged.contains("\"ok\":true"));
        assert!(!logged.contains(note));
        assert!(!logged.contains(&thread_id.to_string()));
        assert!(!logged.contains("[17,34,51,68]"));
        assert!(!logged.contains("report-id"));
    }
}

fn write_record<T>(dir: &str, kind: &str, obj: &T)
where
    T: Serialize,
{
    let value = json!({
        "ts": now_ts(),
        "dir": dir,
        "kind": kind,
        "payload": obj,
    });
    LOGGER.write_json_line(value);
}
