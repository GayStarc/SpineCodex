use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::fs::Metadata;
use std::fs::OpenOptions;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Cursor;
use std::io::Write;
use std::path::Path;
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
use codex_core::RolloutDebugRedactorError;
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
use image::DynamicImage;
use image::ImageDecoder;
use image::ImageEncoder;
use image::ImageFormat;
use image::ImageReader;
use image::Limits;
use image::RgbaImage;
use image::codecs::png::PngEncoder;
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
const MAX_SCREENSHOT_DECODE_ALLOC_BYTES: u64 = MAX_SCREENSHOT_PIXELS * 8;
const MAX_SCREENSHOT_BASE64_BYTES: usize = ((MAX_SCREENSHOT_BYTES + 2) / 3) * 4 + 4;
const MAX_SOURCE_LINE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PACKAGE_SOURCE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PACKAGE_TRACKED_THREAD_IDS: usize = 131_072;
const MAX_PACKAGE_SOURCE_RECORDS: u64 = MAX_PACKAGE_TRACKED_THREAD_IDS as u64;
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
    validate_subtree_thread_count(subtree_thread_ids.len()).map_err(map_bundle_error)?;
    let subtree_thread_ids = normalize_subtree_thread_ids(root_thread_id, subtree_thread_ids);
    let parent_thread_ids =
        resolve_parent_thread_ids(&thread_manager, state_db.as_ref(), &subtree_thread_ids).await;
    let captures = capture_rollout_sources(&thread_manager, state_db.as_ref(), &subtree_thread_ids)
        .await
        .map_err(map_bundle_error)?;

    let rollout_limit = SPINE_FEEDBACK_MAX_ATTACHMENT_BYTES
        .checked_sub(screenshot_bytes)
        .ok_or_else(|| invalid_request("Spine feedback screenshots exceed the attachment limit"))?;
    let rollout_bytes = tokio::task::spawn_blocking(move || {
        build_rollout_debug_attachment(
            root_thread_id,
            captures,
            parent_thread_ids,
            BundleBuildLimits::production(rollout_limit),
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

pub(super) fn spine_feedback_enabled(thread: &codex_core::CodexThread) -> bool {
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

fn validate_subtree_thread_count(thread_count: usize) -> Result<(), BundleBuildError> {
    if thread_count > MAX_PACKAGE_TRACKED_THREAD_IDS {
        return Err(BundleBuildError::SourceWorkLimitExceeded {
            resource: "thread identifiers",
            limit: MAX_PACKAGE_TRACKED_THREAD_IDS as u64,
        });
    }
    Ok(())
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
) -> Result<Vec<CapturedThread>, BundleBuildError> {
    capture_rollout_sources_with_limit(
        thread_manager,
        state_db,
        thread_ids,
        MAX_PACKAGE_SOURCE_BYTES,
    )
    .await
}

async fn capture_rollout_sources_with_limit(
    thread_manager: &ThreadManager,
    state_db: Option<&StateDbHandle>,
    thread_ids: &[ThreadId],
    source_bytes_limit: u64,
) -> Result<Vec<CapturedThread>, BundleBuildError> {
    let mut captures = Vec::with_capacity(thread_ids.len());
    let mut captured_source_bytes = 0_u64;
    for thread_id in thread_ids {
        let source = match thread_manager.get_thread(*thread_id).await {
            Ok(thread) => {
                if thread.flush_rollout().await.is_err() {
                    CapturedSource::FlushFailed
                } else if let Some(path) = thread.rollout_path() {
                    capture_path(path).await?
                } else {
                    CapturedSource::Missing
                }
            }
            Err(_) => match state_db {
                Some(state_db) => match state_db
                    .find_rollout_path_by_id(*thread_id, /*archived_only*/ None)
                    .await
                {
                    Ok(Some(path)) => capture_path(path).await?,
                    Ok(None) => CapturedSource::Missing,
                    Err(_) => CapturedSource::Unavailable,
                },
                None => CapturedSource::Unavailable,
            },
        };
        if let CapturedSource::Ready(snapshot) = &source {
            captured_source_bytes = captured_source_bytes
                .checked_add(snapshot.captured_bytes)
                .ok_or(BundleBuildError::SourceWorkLimitExceeded {
                    resource: "captured bytes",
                    limit: source_bytes_limit,
                })?;
            if captured_source_bytes > source_bytes_limit {
                return Err(BundleBuildError::SourceWorkLimitExceeded {
                    resource: "captured bytes",
                    limit: source_bytes_limit,
                });
            }
        }
        captures.push(CapturedThread {
            thread_id: *thread_id,
            source,
        });
    }
    Ok(captures)
}

async fn capture_path(path: PathBuf) -> Result<CapturedSource, BundleBuildError> {
    match tokio::task::spawn_blocking(move || snapshot_rollout_source(&path)).await {
        Ok(source) => source,
        Err(_) => Ok(CapturedSource::Unavailable),
    }
}

fn rollout_source_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    options
}

fn snapshot_rollout_source(path: &Path) -> Result<CapturedSource, BundleBuildError> {
    let file = match rollout_source_open_options().open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(CapturedSource::Missing),
        Err(err) if is_source_capture_resource_exhaustion(&err) => {
            return Err(BundleBuildError::SourceCaptureResourceExhausted(err));
        }
        Err(_) => return Ok(CapturedSource::Unreadable),
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(err) if is_source_capture_resource_exhaustion(&err) => {
            return Err(BundleBuildError::SourceCaptureResourceExhausted(err));
        }
        Err(_) => return Ok(CapturedSource::Unreadable),
    };
    if !metadata.is_file() {
        return Ok(CapturedSource::Unreadable);
    }
    let identity = RolloutSourceIdentity::from_metadata(&metadata)
        .map_err(BundleBuildError::SourceIdentityUnavailable)?;
    Ok(CapturedSource::Ready(CapturedFileSnapshot {
        path: path.to_path_buf(),
        captured_bytes: metadata.len(),
        identity,
    }))
}

fn is_source_capture_resource_exhaustion(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::OutOfMemory {
        return true;
    }
    #[cfg(unix)]
    {
        return matches!(
            error.raw_os_error(),
            Some(libc::EMFILE | libc::ENFILE | libc::ENOMEM)
        );
    }
    #[cfg(windows)]
    {
        return matches!(error.raw_os_error(), Some(4 | 8 | 14));
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

fn reopen_rollout_source(snapshot: &CapturedFileSnapshot) -> io::Result<File> {
    let file = rollout_source_open_options().open(&snapshot.path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "captured rollout source is no longer a regular file",
        ));
    }
    if RolloutSourceIdentity::from_metadata(&metadata)? != snapshot.identity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "captured rollout source identity changed",
        ));
    }
    if metadata.len() < snapshot.captured_bytes {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "captured rollout source shrank",
        ));
    }
    Ok(file)
}

#[derive(Debug)]
struct CapturedThread {
    thread_id: ThreadId,
    source: CapturedSource,
}

#[derive(Debug)]
enum CapturedSource {
    Ready(CapturedFileSnapshot),
    Missing,
    FlushFailed,
    Unavailable,
    Unreadable,
}

#[derive(Debug)]
struct CapturedFileSnapshot {
    path: PathBuf,
    captured_bytes: u64,
    identity: RolloutSourceIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RolloutSourceIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
}

impl RolloutSourceIdentity {
    fn from_metadata(metadata: &Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            Ok(Self::Unix {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stable rollout source identity is unavailable on this platform",
            ))
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct BundleBuildLimits {
    output_bytes: usize,
    source_line_bytes: usize,
    source_bytes: u64,
    source_records: u64,
}

impl BundleBuildLimits {
    const fn production(output_bytes: usize) -> Self {
        Self {
            output_bytes,
            source_line_bytes: MAX_SOURCE_LINE_BYTES,
            source_bytes: MAX_PACKAGE_SOURCE_BYTES,
            source_records: MAX_PACKAGE_SOURCE_RECORDS,
        }
    }
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
    captures: Vec<CapturedThread>,
    parent_thread_ids: HashMap<ThreadId, ThreadId>,
    limits: BundleBuildLimits,
) -> Result<Vec<u8>, BundleBuildError> {
    let total_captured_bytes = captures.iter().try_fold(0_u64, |total, capture| {
        let captured_bytes = match &capture.source {
            CapturedSource::Ready(snapshot) => snapshot.captured_bytes,
            CapturedSource::Missing
            | CapturedSource::FlushFailed
            | CapturedSource::Unavailable
            | CapturedSource::Unreadable => 0,
        };
        total
            .checked_add(captured_bytes)
            .ok_or(BundleBuildError::SourceWorkLimitExceeded {
                resource: "captured bytes",
                limit: limits.source_bytes,
            })
    })?;
    if total_captured_bytes > limits.source_bytes {
        return Err(BundleBuildError::SourceWorkLimitExceeded {
            resource: "captured bytes",
            limit: limits.source_bytes,
        });
    }

    let mut redactor = RolloutDebugRedactor::default();
    let mut local_thread_ids = HashMap::with_capacity(captures.len());
    for capture in &captures {
        let local_id = redactor
            .register_thread_id(&capture.thread_id.to_string())
            .map_err(BundleBuildError::Redaction)?;
        local_thread_ids.insert(capture.thread_id, local_id);
    }
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
    let capped = CappedWriter::new(limits.output_bytes, Arc::clone(&exceeded));
    let mut gzip = GzBuilder::new()
        .mtime(0)
        .write(capped, Compression::default());
    write_json_line(&mut gzip, &manifest, limits.output_bytes, &exceeded)?;

    let mut source_records = 0_u64;
    for capture in captures {
        let CapturedSource::Ready(snapshot) = capture.source else {
            continue;
        };
        let file = reopen_rollout_source(&snapshot).map_err(BundleBuildError::SourceRead)?;
        let mut reader = BufReader::with_capacity(ROLLOUT_READER_CAPACITY, file);
        let mut remaining = snapshot.captured_bytes;
        let mut ordinal = 0_u64;
        while let Some(line) =
            read_bounded_source_line(&mut reader, &mut remaining, limits.source_line_bytes)
                .map_err(BundleBuildError::SourceRead)?
        {
            source_records =
                source_records
                    .checked_add(1)
                    .ok_or(BundleBuildError::SourceWorkLimitExceeded {
                        resource: "records",
                        limit: limits.source_records,
                    })?;
            if source_records > limits.source_records {
                return Err(BundleBuildError::SourceWorkLimitExceeded {
                    resource: "records",
                    limit: limits.source_records,
                });
            }
            let item = match line {
                BoundedSourceLine::Retained(line) => redactor
                    .redact_json_line_to_value(line.as_slice())
                    .map_err(BundleBuildError::Redaction)?,
                BoundedSourceLine::Oversized => RolloutDebugRedactor::oversized_value(),
            };
            let record = RolloutDebugThreadRecord {
                record_type: "thread_record",
                thread_local_id: local_thread_ids[&capture.thread_id],
                ordinal,
                item,
            };
            write_json_line(&mut gzip, &record, limits.output_bytes, &exceeded)?;
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
                limit: limits.output_bytes,
            }
        } else {
            BundleBuildError::Encoding(err)
        }
    })?;
    Ok(capped.into_inner())
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
        CapturedSource::Ready(snapshot) => ManifestSource::Ready {
            captured_bytes: snapshot.captured_bytes,
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
    #[error("rollout debug source {resource} exceeds package limit {limit}")]
    SourceWorkLimitExceeded { resource: &'static str, limit: u64 },
    #[error("rollout source capture exhausted process resources")]
    SourceCaptureResourceExhausted(#[source] io::Error),
    #[error("stable rollout source identity is unavailable")]
    SourceIdentityUnavailable(#[source] io::Error),
    #[error("failed to read captured rollout source")]
    SourceRead(#[source] io::Error),
    #[error("failed to encode rollout debug attachment")]
    Encoding(#[source] io::Error),
    #[error("failed to serialize rollout debug record")]
    Serialization(#[source] serde_json::Error),
    #[error("rollout debug redaction state limit exceeded")]
    Redaction(#[source] RolloutDebugRedactorError),
}

fn map_bundle_error(error: BundleBuildError) -> JSONRPCErrorError {
    match error {
        BundleBuildError::AttachmentTooLarge { .. }
        | BundleBuildError::SourceWorkLimitExceeded { .. } => invalid_request(error.to_string()),
        BundleBuildError::SourceRead(_)
        | BundleBuildError::SourceCaptureResourceExhausted(_)
        | BundleBuildError::SourceIdentityUnavailable(_)
        | BundleBuildError::Encoding(_)
        | BundleBuildError::Serialization(_)
        | BundleBuildError::Redaction(_) => internal_error(error.to_string()),
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

        let image = decode_screenshot_png(index, &input)?;
        let normalized = encode_screenshot_png(index, &image.into_rgba8(), MAX_SCREENSHOT_BYTES)?;
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

fn screenshot_decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SCREENSHOT_SIDE);
    limits.max_image_height = Some(MAX_SCREENSHOT_SIDE);
    limits.max_alloc = Some(MAX_SCREENSHOT_DECODE_ALLOC_BYTES);
    limits
}

fn decode_screenshot_png(index: usize, input: &[u8]) -> Result<DynamicImage, String> {
    let mut reader = ImageReader::with_format(Cursor::new(input), ImageFormat::Png);
    reader.limits(screenshot_decode_limits());
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| format!("screenshot {} is not a valid PNG image", index + 1))?;
    let dimensions = decoder.dimensions();
    validate_screenshot_dimensions(index, dimensions)?;

    let mut remaining_limits = screenshot_decode_limits();
    remaining_limits
        .reserve(decoder.total_bytes())
        .map_err(|_| format!("screenshot {} requires too much decode memory", index + 1))?;
    decoder
        .set_limits(remaining_limits)
        .map_err(|_| format!("screenshot {} exceeds decode limits", index + 1))?;
    DynamicImage::from_decoder(decoder)
        .map_err(|_| format!("screenshot {} is not a valid PNG image", index + 1))
}

fn encode_screenshot_png(index: usize, image: &RgbaImage, limit: usize) -> Result<Vec<u8>, String> {
    let exceeded = Arc::new(AtomicBool::new(false));
    let mut output = CappedWriter::new(limit, Arc::clone(&exceeded));
    let encode_result = PngEncoder::new(&mut output).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    );
    if exceeded.load(Ordering::Relaxed) {
        return Err(format!(
            "normalized screenshot {} exceeds {limit} bytes",
            index + 1
        ));
    }
    encode_result.map_err(|_| format!("screenshot {} could not be normalized", index + 1))?;
    Ok(output.into_inner())
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
    use std::collections::HashSet;
    use std::fs;
    use std::fs::OpenOptions;
    use std::io;
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Read;
    use std::io::Write;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::SystemTime;

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
    use sha2::Digest;
    use sha2::Sha256;
    use tempfile::TempDir;

    use super::*;

    const SECRET: &str = "private-spine-feedback-secret";
    const ACCEPTED_REAL_CORPUS_THREADS: usize = 24;
    const ACCEPTED_REAL_CORPUS_DIRECT_CHILDREN: usize = 23;
    const ACCEPTED_REAL_CORPUS_BYTES: u64 = 106_713_621;
    const ACCEPTED_REAL_CORPUS_RECORDS: u64 = 35_612;
    const STAGING_UUID_ROOT: &str = "01911111-1111-7111-8111-111111111111";
    const STAGING_UUID_CHILD: &str = "01922222-2222-7222-8222-222222222222";
    const STAGING_HOME_PATH: &str = "/home/spine-feedback-staging/private.rs";
    const STAGING_DATA_PATH: &str = "/data/spine-feedback-staging/private.json";
    const STAGING_HTTP_URL: &str = "https://staging.invalid/private?token=canary";
    const STAGING_FILE_URL: &str = "file:///home/spine-feedback-staging/private.rs";
    const STAGING_SECRET: &str = "spine-feedback-staging-secret-canary";
    const STAGING_NONCE: &str = "v1";

    fn bundle_limits(output_bytes: usize, source_line_bytes: usize) -> BundleBuildLimits {
        BundleBuildLimits {
            output_bytes,
            source_line_bytes,
            ..BundleBuildLimits::production(output_bytes)
        }
    }

    #[derive(Debug)]
    struct TestSessionFile {
        thread_id: ThreadId,
        parent_thread_id: Option<ThreadId>,
        path: PathBuf,
        metadata: TestFileMetadata,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestFileMetadata {
        len: u64,
        modified: SystemTime,
    }

    fn required_env_path(name: &str) -> PathBuf {
        std::env::var_os(name)
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("{name} must be set for this ignored test"))
    }

    fn validation_temp_null_root() -> PathBuf {
        let cachetree_root = std::env::var_os("CACHETREE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .ancestors()
                    .nth(5)
                    .expect("app-server manifest must be nested under CacheTree")
                    .to_path_buf()
            });
        cachetree_root
            .join("temp/null")
            .canonicalize()
            .expect("CacheTree temp/null must exist")
    }

    fn validation_target_under(
        temp_null_root: &Path,
        requested_path: &Path,
    ) -> io::Result<PathBuf> {
        if !requested_path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "validation output path must be absolute",
            ));
        }
        let canonical_root = temp_null_root.canonicalize()?;
        let requested_parent = requested_path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "validation output path must have a parent",
            )
        })?;
        let canonical_parent = requested_parent.canonicalize()?;
        if !canonical_parent.starts_with(&canonical_root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "validation output must remain under CacheTree temp/null",
            ));
        }
        let file_name = requested_path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "validation output path must have a final component",
            )
        })?;
        Ok(canonical_parent.join(file_name))
    }

    fn validation_output_file(requested_path: &Path) -> PathBuf {
        validation_target_under(&validation_temp_null_root(), requested_path)
            .expect("validation output file must be contained by CacheTree temp/null")
    }

    fn create_validation_output_dir_at(
        temp_null_root: &Path,
        requested_path: &Path,
    ) -> io::Result<PathBuf> {
        let output_dir = validation_target_under(temp_null_root, requested_path)?;
        fs::create_dir(&output_dir)?;
        Ok(output_dir)
    }

    fn create_validation_output_dir(requested_path: &Path) -> PathBuf {
        create_validation_output_dir_at(&validation_temp_null_root(), requested_path)
            .expect("staging output must be a new directory under CacheTree temp/null")
    }

    fn file_metadata(metadata: &Metadata) -> TestFileMetadata {
        TestFileMetadata {
            len: metadata.len(),
            modified: metadata
                .modified()
                .expect("rollout source must expose modification time"),
        }
    }

    fn source_metadata(path: &Path) -> TestFileMetadata {
        let metadata = fs::metadata(path).expect("read rollout source metadata");
        assert!(metadata.is_file(), "rollout source must be a regular file");
        file_metadata(&metadata)
    }

    fn nested_parent_thread_id(value: &Value) -> Option<ThreadId> {
        match value {
            Value::Object(fields) => {
                if let Some(Value::String(parent_thread_id)) = fields.get("parent_thread_id") {
                    return ThreadId::from_string(parent_thread_id).ok();
                }
                fields.values().find_map(nested_parent_thread_id)
            }
            Value::Array(values) => values.iter().find_map(nested_parent_thread_id),
            _ => None,
        }
    }

    fn read_test_session_file(path: &Path) -> Option<TestSessionFile> {
        let file = File::open(path).ok()?;
        let mut reader = BufReader::new(file);
        let mut first_line = String::new();
        reader.read_line(&mut first_line).ok()?;
        let record: Value = serde_json::from_str(&first_line).ok()?;
        if record.get("type").and_then(Value::as_str) != Some("session_meta") {
            return None;
        }
        let payload = record.get("payload")?;
        let thread_id = payload
            .get("id")
            .and_then(Value::as_str)
            .and_then(|value| ThreadId::from_string(value).ok())?;
        let parent_thread_id = payload
            .get("parent_thread_id")
            .and_then(Value::as_str)
            .and_then(|value| ThreadId::from_string(value).ok())
            .or_else(|| payload.get("source").and_then(nested_parent_thread_id));
        Some(TestSessionFile {
            thread_id,
            parent_thread_id,
            path: path.to_path_buf(),
            metadata: source_metadata(path),
        })
    }

    fn collect_rollout_paths(directory: &Path, output: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).expect("read sessions directory") {
            let entry = entry.expect("read sessions directory entry");
            let file_type = entry.file_type().expect("read sessions entry type");
            if file_type.is_dir() {
                collect_rollout_paths(&entry.path(), output);
            } else if file_type.is_file() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                    output.push(entry.path());
                }
            }
        }
    }

    fn discover_real_corpus(root_path: &Path, sessions_root: &Path) -> Vec<TestSessionFile> {
        let root = read_test_session_file(root_path).expect("root rollout must be valid");
        let root_thread_id = root.thread_id;
        let mut paths = Vec::new();
        collect_rollout_paths(sessions_root, &mut paths);
        paths.sort();

        let mut by_id = HashMap::new();
        for path in paths {
            let Some(session) = read_test_session_file(&path) else {
                continue;
            };
            assert!(
                by_id.insert(session.thread_id, session).is_none(),
                "duplicate rollout thread id"
            );
        }
        by_id.entry(root_thread_id).or_insert(root);

        let mut children = HashMap::<ThreadId, Vec<ThreadId>>::new();
        for session in by_id.values() {
            if let Some(parent_thread_id) = session.parent_thread_id {
                children
                    .entry(parent_thread_id)
                    .or_default()
                    .push(session.thread_id);
            }
        }
        for child_ids in children.values_mut() {
            child_ids.sort_unstable_by_key(ToString::to_string);
        }

        let mut pending = vec![root_thread_id];
        let mut seen = HashSet::new();
        while let Some(thread_id) = pending.pop() {
            assert!(seen.insert(thread_id), "cycle in rollout thread tree");
            if let Some(child_ids) = children.get(&thread_id) {
                pending.extend(child_ids.iter().rev().copied());
            }
        }

        let mut descendants = seen
            .into_iter()
            .filter(|thread_id| *thread_id != root_thread_id)
            .collect::<Vec<_>>();
        descendants.sort_unstable_by_key(ToString::to_string);
        let mut ordered_ids = Vec::with_capacity(descendants.len() + 1);
        ordered_ids.push(root_thread_id);
        ordered_ids.extend(descendants);
        ordered_ids
            .into_iter()
            .map(|thread_id| {
                by_id
                    .remove(&thread_id)
                    .expect("discovered rollout must remain indexed")
            })
            .collect()
    }

    fn count_source_records(path: &Path) -> u64 {
        let file = File::open(path).expect("open rollout source for record count");
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut count = 0_u64;
        loop {
            line.clear();
            let read = reader
                .read_until(b'\n', &mut line)
                .expect("count rollout source records");
            if read == 0 {
                return count;
            }
            count = count.saturating_add(1);
        }
    }

    fn create_new_file(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("create validation output without overwriting");
        file.write_all(bytes).expect("write validation output");
        file.flush().expect("flush validation output");
    }

    fn checkerboard_png() -> Vec<u8> {
        let image = ImageBuffer::from_fn(8, 8, |x, y| {
            if (x + y) % 2 == 0 {
                Rgba([18, 52, 86, 255])
            } else {
                Rgba([240, 220, 200, 255])
            }
        });
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("encode checkerboard PNG");
        bytes.into_inner()
    }

    fn encode_jsonl(records: &[Value]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, record).expect("serialize synthetic rollout record");
            bytes.push(b'\n');
        }
        bytes
    }

    fn assert_line_has_no_private_patterns(line: &str) {
        for pattern in ["/home/", "/data/", "http://", "https://", "file://"] {
            assert!(
                !line.contains(pattern),
                "rollout debug line leaked a private pattern"
            );
        }
    }

    fn thread_id(index: u8) -> ThreadId {
        ThreadId::from_string(&format!("01900000-0000-7000-8000-{index:012x}"))
            .expect("valid thread id")
    }

    fn write_source(tempdir: &TempDir, name: &str, bytes: &[u8]) -> CapturedSource {
        let path = tempdir.path().join(name);
        std::fs::write(&path, bytes).expect("write source");
        match snapshot_rollout_source(&path).expect("snapshot source") {
            source @ CapturedSource::Ready(_) => source,
            source => panic!("expected ready source, got {source:?}"),
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
        let mut parents = (1..10)
            .map(|index| {
                let parent = if index == 1 {
                    root
                } else {
                    thread_id(index - 1)
                };
                (thread_id(index), parent)
            })
            .collect::<HashMap<_, _>>();
        let late_child = thread_id(10);
        parents.insert(late_child, root);

        let first = build_rollout_debug_attachment(
            root,
            captures,
            parents,
            bundle_limits(1024 * 1024, 512),
        )
        .expect("build attachment");
        let lines = decode_lines(&first);
        let decompressed = serde_json::to_string(&lines).expect("serialize lines");
        assert!(!decompressed.contains(SECRET));
        for index in 0..10 {
            assert!(!decompressed.contains(&thread_id(index).to_string()));
        }
        assert!(!decompressed.contains(&late_child.to_string()));

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
    fn captured_snapshot_excludes_appended_suffix() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = thread_id(0);
        let source_path = tempdir.path().join("captured.jsonl");
        let original = b"{\"timestamp\":\"x\"}\n";
        std::fs::write(&source_path, original).expect("write original source");
        let source = snapshot_rollout_source(&source_path).expect("snapshot original source");

        OpenOptions::new()
            .append(true)
            .open(&source_path)
            .expect("open original inode for append")
            .write_all(
                format!(
                    "{{\"timestamp\":\"x\",\"type\":\"future\",\"payload\":{{\"secret\":\"{SECRET}\"}}}}\n"
            )
            .as_bytes(),
        )
        .expect("append after capture boundary");

        let bytes = build_rollout_debug_attachment(
            root,
            vec![CapturedThread {
                thread_id: root,
                source,
            }],
            HashMap::new(),
            bundle_limits(1024 * 1024, 1024),
        )
        .expect("build from captured snapshot");
        let lines = decode_lines(&bytes);
        let encoded = serde_json::to_string(&lines).expect("serialize output");

        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0]["threads"][0]["source"]["captured_bytes"],
            original.len()
        );
        assert_eq!(lines[1]["ordinal"], 0);
        assert_eq!(lines[1]["item"]["record_type"], "malformed_redacted");
        assert!(!encoded.contains(SECRET));
    }

    #[test]
    fn captured_snapshot_rejects_path_replacement() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = thread_id(0);
        let source_path = tempdir.path().join("captured.jsonl");
        let moved_path = tempdir.path().join("captured-original.jsonl");
        std::fs::write(&source_path, b"{\"timestamp\":\"x\"}\n").expect("write original source");
        let source = snapshot_rollout_source(&source_path).expect("snapshot original source");

        std::fs::rename(&source_path, &moved_path).expect("move captured inode");
        std::fs::write(
            &source_path,
            b"{\"timestamp\":\"x\",\"type\":\"future\",\"payload\":{}}\n",
        )
        .expect("write replacement inode");

        let error = build_rollout_debug_attachment(
            root,
            vec![CapturedThread {
                thread_id: root,
                source,
            }],
            HashMap::new(),
            bundle_limits(1024 * 1024, 1024),
        )
        .expect_err("path replacement must invalidate the package");
        assert!(matches!(error, BundleBuildError::SourceRead(_)));
    }

    #[test]
    fn rollout_source_requires_a_regular_file_snapshot() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            snapshot_rollout_source(tempdir.path()).expect("snapshot directory"),
            CapturedSource::Unreadable
        ));
        assert!(matches!(
            snapshot_rollout_source(&tempdir.path().join("missing.jsonl"))
                .expect("snapshot missing source"),
            CapturedSource::Missing
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rollout_source_rejects_symlink_and_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;
        use std::time::Duration;
        use std::time::Instant;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let target = tempdir.path().join("target.jsonl");
        let link = tempdir.path().join("link.jsonl");
        std::fs::write(&target, b"{}\n").expect("write symlink target");
        symlink(&target, &link).expect("create symlink");
        assert!(matches!(
            snapshot_rollout_source(&link).expect("snapshot symlink"),
            CapturedSource::Unreadable
        ));

        let fifo = tempdir.path().join("rollout.fifo");
        let fifo_path =
            CString::new(fifo.as_os_str().as_bytes()).expect("fifo path contains no nul");
        // SAFETY: `fifo_path` is a valid, nul-terminated path and `mkfifo`
        // does not retain the pointer after returning.
        let result = unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) };
        assert_eq!(result, 0);
        let started = Instant::now();
        assert!(matches!(
            snapshot_rollout_source(&fifo).expect("snapshot FIFO"),
            CapturedSource::Unreadable
        ));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "FIFO open must not block"
        );

        assert!(
            is_source_capture_resource_exhaustion(&io::Error::from_raw_os_error(libc::EMFILE)),
            "descriptor exhaustion must fail the package rather than become Unreadable"
        );
        assert!(
            is_source_capture_resource_exhaustion(&io::Error::from_raw_os_error(libc::ENFILE)),
            "system descriptor exhaustion must fail the package"
        );
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

        let first = build_rollout_debug_attachment(
            root,
            make_captures(),
            HashMap::new(),
            bundle_limits(4096, 4096),
        )
        .expect("first build");
        let second = build_rollout_debug_attachment(
            root,
            make_captures(),
            HashMap::new(),
            bundle_limits(4096, 4096),
        )
        .expect("second build");
        assert_eq!(first, second);

        let error = build_rollout_debug_attachment(
            root,
            make_captures(),
            HashMap::new(),
            bundle_limits(1, 4096),
        )
        .expect_err("tiny output cap must fail");
        assert!(matches!(
            error,
            BundleBuildError::AttachmentTooLarge { limit: 1 }
        ));
    }

    #[test]
    fn bundle_fails_closed_on_package_source_byte_and_record_limits() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = thread_id(0);
        let source = b"{}\n{}\n{}\n";
        let make_captures = || {
            vec![CapturedThread {
                thread_id: root,
                source: write_source(&tempdir, "work-limits.jsonl", source),
            }]
        };

        let byte_error = build_rollout_debug_attachment(
            root,
            make_captures(),
            HashMap::new(),
            BundleBuildLimits {
                source_bytes: u64::try_from(source.len() - 1).expect("source length fits u64"),
                ..bundle_limits(4096, 4096)
            },
        )
        .expect_err("captured byte budget must fail before source scanning");
        assert!(matches!(
            byte_error,
            BundleBuildError::SourceWorkLimitExceeded {
                resource: "captured bytes",
                ..
            }
        ));

        let record_error = build_rollout_debug_attachment(
            root,
            make_captures(),
            HashMap::new(),
            BundleBuildLimits {
                source_records: 2,
                ..bundle_limits(4096, 4096)
            },
        )
        .expect_err("record work budget must fail the entire package");
        assert!(matches!(
            record_error,
            BundleBuildError::SourceWorkLimitExceeded {
                resource: "records",
                limit: 2
            }
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
        assert!(
            normalize_screenshots(vec![SpineFeedbackScreenshot {
                png_base64: BASE64_STANDARD.encode(png(MAX_SCREENSHOT_SIDE + 1, 1)),
            }])
            .is_err()
        );
    }

    #[test]
    fn screenshot_png_encoding_stops_at_the_configured_limit() {
        let image = ImageBuffer::from_pixel(2, 2, Rgba([1, 2, 3, 255]));
        let error = encode_screenshot_png(0, &image, 1).expect_err("tiny cap must stop encoding");
        assert!(error.contains("exceeds 1 bytes"));
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
    fn subtree_thread_count_fails_before_unbounded_package_work() {
        validate_subtree_thread_count(MAX_PACKAGE_TRACKED_THREAD_IDS)
            .expect("the tracked-ID boundary is accepted");

        let error = validate_subtree_thread_count(MAX_PACKAGE_TRACKED_THREAD_IDS + 1)
            .expect_err("a subtree larger than the tracked-ID budget must fail");
        assert!(matches!(
            error,
            BundleBuildError::SourceWorkLimitExceeded {
                resource: "thread identifiers",
                limit,
            } if limit == MAX_PACKAGE_TRACKED_THREAD_IDS as u64
        ));
    }

    #[cfg(not(unix))]
    #[test]
    fn platform_without_stable_file_identity_fails_closed() {
        let file = tempfile::tempfile().expect("create temporary source");
        let metadata = file.metadata().expect("read source metadata");

        let error = RolloutSourceIdentity::from_metadata(&metadata)
            .expect_err("weak metadata must not stand in for file identity");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
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
        let first = redactor
            .register_thread_id("raw-thread-a")
            .expect("register first thread");
        let second = redactor
            .register_thread_id("raw-thread-b")
            .expect("register second thread");
        assert_eq!(
            first,
            redactor
                .register_thread_id("raw-thread-a")
                .expect("reuse first thread")
        );
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

    #[test]
    fn validation_output_paths_require_canonical_temp_null_containment() {
        let workspace = TempDir::new().expect("create validation path workspace");
        let temp_null = workspace.path().join("temp/null");
        let valid_parent = temp_null.join("valid");
        let escaped_parent = workspace.path().join("temp/outside");
        fs::create_dir_all(&valid_parent).expect("create valid validation parent");
        fs::create_dir_all(&escaped_parent).expect("create escaped validation parent");

        let valid = validation_target_under(&temp_null, &valid_parent.join("report.json"))
            .expect("accept a canonically contained output");
        assert_eq!(
            valid,
            valid_parent
                .canonicalize()
                .expect("canonicalize valid parent")
                .join("report.json")
        );

        let escaped = temp_null.join("../outside/report.json");
        assert!(
            validation_target_under(&temp_null, &escaped).is_err(),
            "a lexical temp/null prefix must not permit .. escape"
        );
        assert!(
            validation_target_under(&temp_null, Path::new("relative/report.json")).is_err(),
            "validation output paths must be absolute"
        );
    }

    #[cfg(unix)]
    #[test]
    fn validation_output_path_rejects_temp_null_symlink_escape() {
        use std::os::unix::fs::symlink;

        let workspace = TempDir::new().expect("create symlink validation workspace");
        let temp_null = workspace.path().join("temp/null");
        let outside = workspace.path().join("outside");
        fs::create_dir_all(&temp_null).expect("create temp/null root");
        fs::create_dir(&outside).expect("create outside directory");
        symlink(&outside, temp_null.join("escape")).expect("create escape symlink");

        assert!(
            validation_target_under(&temp_null, &temp_null.join("escape/report.json")).is_err(),
            "a symlinked parent must not escape canonical temp/null"
        );
    }

    #[test]
    fn staging_validation_output_requires_absent_directory() {
        let workspace = TempDir::new().expect("create staging validation workspace");
        let temp_null = workspace.path().join("temp/null");
        fs::create_dir_all(&temp_null).expect("create temp/null root");
        let output_dir = temp_null.join("staging");

        let created = create_validation_output_dir_at(&temp_null, &output_dir)
            .expect("create a fresh staging output directory");
        assert_eq!(
            created,
            temp_null
                .canonicalize()
                .expect("canonicalize temp/null root")
                .join("staging")
        );
        assert!(
            create_validation_output_dir_at(&temp_null, &output_dir).is_err(),
            "an existing staging output directory must be rejected"
        );
    }

    #[test]
    #[ignore = "requires the private accepted 24-thread rollout corpus"]
    fn real_corpus_bundle_matches_accepted_structure_and_privacy() {
        let root_path = required_env_path("SPINE_FEEDBACK_REAL_CORPUS_ROOT");
        let sessions_root = required_env_path("SPINE_FEEDBACK_REAL_CORPUS_SESSIONS_ROOT");
        let output_path =
            validation_output_file(&required_env_path("SPINE_FEEDBACK_REAL_CORPUS_OUTPUT"));

        let corpus = discover_real_corpus(&root_path, &sessions_root);
        assert_eq!(corpus.len(), ACCEPTED_REAL_CORPUS_THREADS);
        let root_thread_id = corpus[0].thread_id;
        assert_eq!(
            corpus[0].parent_thread_id, None,
            "accepted corpus root changed"
        );
        assert_eq!(
            corpus
                .iter()
                .filter(|session| session.parent_thread_id == Some(root_thread_id))
                .count(),
            ACCEPTED_REAL_CORPUS_DIRECT_CHILDREN
        );
        assert!(
            corpus[1..]
                .iter()
                .all(|session| session.parent_thread_id == Some(root_thread_id)),
            "accepted corpus topology changed"
        );

        let raw_bytes = corpus
            .iter()
            .map(|session| session.metadata.len)
            .sum::<u64>();
        assert_eq!(raw_bytes, ACCEPTED_REAL_CORPUS_BYTES);
        let source_records = corpus
            .iter()
            .map(|session| count_source_records(&session.path))
            .sum::<u64>();
        assert_eq!(source_records, ACCEPTED_REAL_CORPUS_RECORDS);

        let before = corpus
            .iter()
            .map(|session| (session.path.clone(), session.metadata.clone()))
            .collect::<Vec<_>>();
        let parents = corpus
            .iter()
            .filter_map(|session| {
                session
                    .parent_thread_id
                    .map(|parent_thread_id| (session.thread_id, parent_thread_id))
            })
            .collect::<HashMap<_, _>>();
        let raw_thread_ids = corpus
            .iter()
            .map(|session| session.thread_id.to_string())
            .collect::<Vec<_>>();
        let captures = corpus
            .iter()
            .map(|session| {
                let source =
                    snapshot_rollout_source(&session.path).expect("snapshot accepted source");
                assert!(
                    matches!(&source, CapturedSource::Ready(_)),
                    "accepted rollout source must remain ready"
                );
                CapturedThread {
                    thread_id: session.thread_id,
                    source,
                }
            })
            .collect::<Vec<_>>();

        let bundle = build_rollout_debug_attachment(
            root_thread_id,
            captures,
            parents,
            BundleBuildLimits::production(SPINE_FEEDBACK_MAX_ATTACHMENT_BYTES),
        )
        .expect("build accepted real-corpus attachment");
        assert!(bundle.len() < SPINE_FEEDBACK_MAX_ATTACHMENT_BYTES);

        let mut reader = BufReader::new(GzDecoder::new(bundle.as_slice()));
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("read rollout debug manifest");
        let manifest: Value = serde_json::from_str(&line).expect("parse rollout debug manifest");
        assert_eq!(manifest["record_type"], "manifest");
        assert_eq!(manifest["schema"], ROLLOUT_DEBUG_SCHEMA);
        assert_eq!(manifest["thread_count"], ACCEPTED_REAL_CORPUS_THREADS);
        let manifest_threads = manifest["threads"]
            .as_array()
            .expect("manifest threads must be an array");
        assert_eq!(manifest_threads.len(), ACCEPTED_REAL_CORPUS_THREADS);
        assert_eq!(
            manifest_threads
                .iter()
                .filter(|thread| thread["parent"]["state"] == "root")
                .count(),
            1
        );
        assert_eq!(
            manifest_threads
                .iter()
                .filter(|thread| {
                    thread["parent"]["state"] == "known" && thread["parent"]["thread_local_id"] == 0
                })
                .count(),
            ACCEPTED_REAL_CORPUS_DIRECT_CHILDREN
        );
        assert_eq!(
            manifest_threads
                .iter()
                .map(|thread| {
                    assert_eq!(thread["source"]["state"], "ready");
                    thread["source"]["captured_bytes"]
                        .as_u64()
                        .expect("captured byte count")
                })
                .sum::<u64>(),
            ACCEPTED_REAL_CORPUS_BYTES
        );
        assert_line_has_no_private_patterns(&line);
        for raw_thread_id in &raw_thread_ids {
            assert!(
                !line.contains(raw_thread_id),
                "manifest leaked a raw thread id"
            );
        }

        let mut expected_ordinals = HashMap::<u64, u64>::new();
        let mut emitted_records = 0_u64;
        loop {
            line.clear();
            if reader
                .read_line(&mut line)
                .expect("read rollout debug record")
                == 0
            {
                break;
            }
            assert_line_has_no_private_patterns(&line);
            for raw_thread_id in &raw_thread_ids {
                assert!(
                    !line.contains(raw_thread_id),
                    "record leaked a raw thread id"
                );
            }
            let record: Value =
                serde_json::from_str(&line).expect("parse rollout debug thread record");
            assert_eq!(record["record_type"], "thread_record");
            let local_id = record["thread_local_id"].as_u64().expect("thread local id");
            let ordinal = record["ordinal"].as_u64().expect("record ordinal");
            let expected = expected_ordinals.entry(local_id).or_default();
            assert_eq!(ordinal, *expected, "record ordinals must remain contiguous");
            *expected = expected.saturating_add(1);
            assert!(
                !matches!(
                    record["item"]["record_type"].as_str(),
                    Some("unknown_redacted" | "malformed_redacted" | "oversized_redacted")
                ),
                "accepted corpus unexpectedly emitted a positional placeholder"
            );
            emitted_records = emitted_records.saturating_add(1);
        }
        assert_eq!(emitted_records, ACCEPTED_REAL_CORPUS_RECORDS);

        create_new_file(&output_path, &bundle);

        for (path, expected) in before {
            assert_eq!(
                source_metadata(&path),
                expected,
                "real-corpus validation must not mutate rollout sources"
            );
        }

        let digest = Sha256::digest(&bundle);
        println!(
            "real-corpus package sha256={digest:x} bytes={} threads={} records={emitted_records}",
            bundle.len(),
            ACCEPTED_REAL_CORPUS_THREADS
        );
    }

    #[test]
    #[ignore = "performs one explicit synthetic upload to the SpineCodex Sentry project"]
    fn spine_feedback_staging_upload() {
        assert!(
            matches!(std::env::var("SPINE_FEEDBACK_STAGING").as_deref(), Ok("1")),
            "SPINE_FEEDBACK_STAGING=1 is required for this ignored test"
        );
        let output_dir =
            create_validation_output_dir(&required_env_path("SPINE_FEEDBACK_STAGING_OUTPUT"));
        let synthetic_dir = output_dir.join("synthetic-source");
        fs::create_dir(&synthetic_dir).expect("create owned synthetic source directory");

        let root_thread_id =
            ThreadId::from_string(STAGING_UUID_ROOT).expect("valid staging root thread id");
        let child_thread_id =
            ThreadId::from_string(STAGING_UUID_CHILD).expect("valid staging child thread id");
        let root_records = encode_jsonl(&[
            json!({
                "timestamp": STAGING_SECRET,
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "id": STAGING_UUID_ROOT,
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": format!(
                            "{STAGING_SECRET} {STAGING_HOME_PATH} {STAGING_HTTP_URL}"
                        )
                    }]
                }
            }),
            json!({
                "timestamp": STAGING_SECRET,
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": format!("{STAGING_DATA_PATH} {STAGING_FILE_URL}")
                }
            }),
        ]);
        let child_records = encode_jsonl(&[json!({
            "timestamp": STAGING_SECRET,
            "type": "response_item",
            "payload": {
                "type": "message",
                "id": STAGING_UUID_CHILD,
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": format!(
                        "{STAGING_SECRET} {STAGING_HOME_PATH} {STAGING_DATA_PATH}"
                    )
                }]
            }
        })]);
        let root_source_path = synthetic_dir.join("root.jsonl");
        let child_source_path = synthetic_dir.join("child.jsonl");
        create_new_file(&root_source_path, &root_records);
        create_new_file(&child_source_path, &child_records);
        let root_source =
            snapshot_rollout_source(&root_source_path).expect("snapshot staging root");
        let child_source =
            snapshot_rollout_source(&child_source_path).expect("snapshot staging child");
        assert!(matches!(&root_source, CapturedSource::Ready(_)));
        assert!(matches!(&child_source, CapturedSource::Ready(_)));

        let bundle = build_rollout_debug_attachment(
            root_thread_id,
            vec![
                CapturedThread {
                    thread_id: root_thread_id,
                    source: root_source,
                },
                CapturedThread {
                    thread_id: child_thread_id,
                    source: child_source,
                },
            ],
            HashMap::from([(child_thread_id, root_thread_id)]),
            BundleBuildLimits::production(SPINE_FEEDBACK_MAX_ATTACHMENT_BYTES),
        )
        .expect("build synthetic staging attachment");
        let mut decoded = String::new();
        GzDecoder::new(bundle.as_slice())
            .read_to_string(&mut decoded)
            .expect("decode synthetic staging attachment");
        let decoded_lines = decoded
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("parse staging debug line"))
            .collect::<Vec<_>>();
        assert_eq!(decoded_lines.len(), 4);
        assert_eq!(decoded_lines[0]["record_type"], "manifest");
        assert_eq!(decoded_lines[0]["thread_count"], 2);
        assert_eq!(decoded_lines[0]["threads"][0]["parent"]["state"], "root");
        assert_eq!(decoded_lines[0]["threads"][1]["parent"]["state"], "known");
        assert_eq!(
            decoded_lines[0]["threads"][1]["parent"]["thread_local_id"],
            0
        );
        for canary in [
            STAGING_UUID_ROOT,
            STAGING_UUID_CHILD,
            STAGING_HOME_PATH,
            STAGING_DATA_PATH,
            STAGING_HTTP_URL,
            STAGING_FILE_URL,
            STAGING_SECRET,
            "/home/",
            "/data/",
            "http://",
            "https://",
            "file://",
        ] {
            assert!(
                !decoded.contains(canary),
                "synthetic staging bundle leaked a canary class"
            );
        }

        let screenshots = normalize_screenshots(vec![SpineFeedbackScreenshot {
            png_base64: BASE64_STANDARD.encode(checkerboard_png()),
        }])
        .expect("normalize staging checkerboard");
        assert_eq!(screenshots.len(), 1);
        let screenshot = screenshots[0].buffer.clone();
        let rollout_path = output_dir.join(SPINE_ROLLOUT_DEBUG_ATTACHMENT_FILENAME);
        let screenshot_path = output_dir.join(SCREENSHOT_FILENAMES[0]);
        create_new_file(&rollout_path, &bundle);
        create_new_file(&screenshot_path, &screenshot);

        let mut attachments = vec![FeedbackAttachment {
            filename: SPINE_ROLLOUT_DEBUG_ATTACHMENT_FILENAME.to_string(),
            content_type: Some(ROLLOUT_DEBUG_CONTENT_TYPE.to_string()),
            buffer: bundle,
        }];
        attachments.extend(screenshots);
        let note = format!("Spine feedback staging validation {STAGING_NONCE}");
        let report_id = upload_spine_feedback(SpineFeedbackUpload {
            note: Some(&note),
            attachments: &attachments,
        })
        .expect("submit synthetic Spine feedback staging report");
        assert_eq!(report_id.len(), 32);
        assert!(report_id.bytes().all(|byte| byte.is_ascii_hexdigit()));

        println!(
            "staging report_id={report_id} rollout_bytes={} screenshot_bytes={}",
            attachments[0].buffer.len(),
            screenshot.len()
        );

        let receipt = json!({
            "schema": "spine.feedback.staging-receipt.v1",
            "report_id": &report_id,
            "attachments": attachments
                .iter()
                .map(|attachment| json!({
                    "filename": attachment.filename.as_str(),
                    "bytes": attachment.buffer.len(),
                }))
                .collect::<Vec<_>>(),
            "local_privacy": {
                "canary_classes_absent": true,
                "checked": ["thread_uuid", "absolute_path", "http_url", "file_url", "secret"],
            },
            "synthetic_source_recyclable": true,
            "sentry_ui_verification_required": true,
        });
        let receipt_bytes = serde_json::to_vec_pretty(&receipt).expect("serialize staging receipt");
        create_new_file(&output_dir.join("receipt.json"), &receipt_bytes);
    }
}
