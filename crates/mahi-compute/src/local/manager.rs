//! Model download/installation manager.
//!
//! Models live in a flat directory as `<id>.gguf`. Downloads stream to
//! `<id>.gguf.part` and atomically rename on completion, so a `.gguf` file is
//! always complete. Interrupted/cancelled downloads keep their `.part` file
//! and resume via HTTP `Range` on the next attempt. Progress is exposed
//! through a shared [`DownloadState`] (atomics + a tiny status mutex) that an
//! FFI layer can poll from any thread.

use crate::local::catalog::{model_catalog, CatalogEntry};
use crate::local::LocalError;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;

/// A `.gguf` file present in the models directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledModel {
    /// File stem; equals the catalog id for catalog downloads.
    pub id: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    /// The matching catalog entry, if this is a catalog model.
    pub catalog_entry: Option<CatalogEntry>,
}

/// Lifecycle of one download, poll-friendly for FFI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadStatus {
    #[default]
    Idle,
    Downloading,
    Completed,
    Failed(String),
}

/// Shared, thread-safe progress for one download (poll via atomics).
#[derive(Debug, Default)]
pub struct DownloadState {
    bytes_downloaded: AtomicU64,
    total_bytes: AtomicU64,
    cancelled: AtomicBool,
    status: Mutex<DownloadStatus>,
}

impl DownloadState {
    /// Bytes written so far (includes resumed bytes from a prior `.part`).
    pub fn bytes_downloaded(&self) -> u64 {
        self.bytes_downloaded.load(Ordering::Relaxed)
    }

    /// Total expected bytes (0 when unknown).
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    /// Progress in `[0.0, 1.0]`; 0.0 while the total is unknown.
    pub fn progress(&self) -> f32 {
        let total = self.total_bytes();
        if total == 0 {
            return 0.0;
        }
        (self.bytes_downloaded() as f64 / total as f64).clamp(0.0, 1.0) as f32
    }

    /// Current lifecycle status.
    pub fn status(&self) -> DownloadStatus {
        self.lock_status().clone()
    }

    /// Request cancellation; the worker stops at the next chunk boundary,
    /// keeping the `.part` file for a future resume.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn lock_status(&self) -> std::sync::MutexGuard<'_, DownloadStatus> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn set_status(&self, status: DownloadStatus) {
        *self.lock_status() = status;
    }

    pub(crate) fn set_total(&self, total: u64) {
        self.total_bytes.store(total, Ordering::Relaxed);
    }

    pub(crate) fn set_downloaded(&self, bytes: u64) {
        self.bytes_downloaded.store(bytes, Ordering::Relaxed);
    }

    pub(crate) fn add_downloaded(&self, bytes: u64) {
        self.bytes_downloaded.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// Handle to an in-flight download started by [`ModelManager::start_download`].
pub struct DownloadHandle {
    model_id: String,
    state: Arc<DownloadState>,
    task: tokio::task::JoinHandle<()>,
}

impl DownloadHandle {
    /// The catalog id being downloaded.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Shared progress state (clone it out for FFI polling).
    pub fn state(&self) -> Arc<DownloadState> {
        Arc::clone(&self.state)
    }

    /// Request cancellation; the `.part` file is kept for resume and the
    /// status becomes `Failed("download cancelled")`.
    pub fn cancel(&self) {
        self.state.cancel();
    }

    /// Wait for the download to finish and return the terminal status.
    pub async fn wait(self) -> DownloadStatus {
        // The worker never panics in normal operation; if it does, surface
        // that as a failure rather than poisoning the caller.
        if self.task.await.is_err() {
            let status = self.state.status();
            if !matches!(status, DownloadStatus::Failed(_)) {
                self.state
                    .set_status(DownloadStatus::Failed("download task panicked".to_string()));
            }
        }
        self.state.status()
    }
}

/// Manages the models directory: installs, downloads, deletion.
pub struct ModelManager {
    models_dir: PathBuf,
}

impl ModelManager {
    pub fn new(models_dir: impl Into<PathBuf>) -> Self {
        Self {
            models_dir: models_dir.into(),
        }
    }

    pub fn models_dir(&self) -> &Path {
        &self.models_dir
    }

    /// Where `model_id` is (or will be) stored on disk.
    pub fn model_path(&self, model_id: &str) -> PathBuf {
        self.models_dir.join(format!("{model_id}.gguf"))
    }

    /// Scan the models directory for installed `.gguf` files. Catalog models
    /// (matched by file stem == catalog id) carry their [`CatalogEntry`];
    /// arbitrary user-dropped `.gguf` files are reported too. `.part` files
    /// are ignored. Returns an empty list when the directory is missing.
    pub async fn installed(&self) -> Vec<InstalledModel> {
        let catalog = model_catalog();
        let mut models = Vec::new();
        let Ok(mut entries) = tokio::fs::read_dir(&self.models_dir).await else {
            return models;
        };
        while let Ok(Some(dirent)) = entries.next_entry().await {
            let path = dirent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("gguf") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let size_bytes = match dirent.metadata().await {
                Ok(meta) if meta.is_file() => meta.len(),
                _ => continue,
            };
            models.push(InstalledModel {
                id: id.to_string(),
                catalog_entry: catalog.iter().find(|e| e.id == id).cloned(),
                path,
                size_bytes,
            });
        }
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }

    /// Start downloading `entry` in a background task. All outcomes —
    /// including setup errors — surface through the handle's
    /// [`DownloadState`], so FFI callers only ever poll.
    pub async fn start_download(&self, entry: &CatalogEntry) -> DownloadHandle {
        let state = Arc::new(DownloadState::default());
        state.set_status(DownloadStatus::Downloading);
        state.set_total(entry.size_bytes);

        let dest = self.model_path(&entry.id);
        let url = entry.download_url.clone();
        let worker_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            let client = reqwest::Client::new();
            match download_with_resume(&client, &url, &dest, &worker_state).await {
                Ok(()) => worker_state.set_status(DownloadStatus::Completed),
                Err(LocalError::Cancelled) => {
                    worker_state.set_status(DownloadStatus::Failed("download cancelled".into()))
                }
                Err(err) => worker_state.set_status(DownloadStatus::Failed(err.to_string())),
            }
        });

        DownloadHandle {
            model_id: entry.id.clone(),
            state,
            task,
        }
    }

    /// Delete an installed model (and any leftover `.part` file).
    pub async fn delete(&self, model_id: &str) -> Result<(), LocalError> {
        let dest = self.model_path(model_id);
        let part = part_path(&dest);
        let removed_model = remove_if_exists(&dest).await?;
        let removed_part = remove_if_exists(&part).await?;
        if removed_model || removed_part {
            Ok(())
        } else {
            Err(LocalError::ModelNotFound(model_id.to_string()))
        }
    }
}

async fn remove_if_exists(path: &Path) -> Result<bool, LocalError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// `<dest>.part` — the in-progress sibling of a download target.
pub(crate) fn part_path(dest: &Path) -> PathBuf {
    let mut os: OsString = dest.as_os_str().to_owned();
    os.push(".part");
    PathBuf::from(os)
}

/// Parse the total length out of a `Content-Range: bytes <a>-<b>/<total>` header.
pub(crate) fn content_range_total(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .rsplit('/')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Stream `url` to `dest` via `<dest>.part` with HTTP-Range resume and an
/// atomic rename at the end.
///
/// - If `dest` already exists the download is a no-op success.
/// - An existing `.part` resumes with `Range: bytes=<len>-`; servers that
///   ignore the range (HTTP 200) restart cleanly, and HTTP 416 clears the
///   `.part` and retries from scratch once.
/// - Cancellation (via [`DownloadState::cancel`]) returns
///   [`LocalError::Cancelled`] and keeps the `.part` for a later resume.
pub(crate) async fn download_with_resume(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    state: &DownloadState,
) -> Result<(), LocalError> {
    if let Ok(meta) = tokio::fs::metadata(dest).await {
        // Already installed: report as instantly complete.
        state.set_downloaded(meta.len());
        state.set_total(meta.len());
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let part = part_path(dest);

    let mut allow_resume = true;
    loop {
        if state.is_cancelled() {
            return Err(LocalError::Cancelled);
        }
        let resume_from = if allow_resume {
            tokio::fs::metadata(&part)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            let _ = tokio::fs::remove_file(&part).await;
            0
        };

        let mut request = client.get(url);
        if resume_from > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
        }
        let response = request
            .send()
            .await
            .map_err(|e| LocalError::Http(format!("request to {url} failed: {e}")))?;
        let status = response.status();

        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE && allow_resume {
            // Stale/oversized .part: throw it away and restart once.
            allow_resume = false;
            continue;
        }
        if !status.is_success() {
            return Err(LocalError::Http(format!("HTTP {status} downloading {url}")));
        }

        // A 206 honors our range; a 200 means the server restarted from zero.
        let resuming = status == reqwest::StatusCode::PARTIAL_CONTENT && resume_from > 0;
        let total = if resuming {
            content_range_total(response.headers())
                .or_else(|| response.content_length().map(|len| resume_from + len))
        } else {
            response.content_length()
        };
        if let Some(total) = total {
            state.set_total(total);
        }

        let mut file = if resuming {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&part)
                .await?
        } else {
            tokio::fs::File::create(&part).await?
        };
        state.set_downloaded(if resuming { resume_from } else { 0 });

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            if state.is_cancelled() {
                file.flush().await?;
                return Err(LocalError::Cancelled);
            }
            let bytes = chunk.map_err(|e| LocalError::Http(format!("stream error: {e}")))?;
            file.write_all(&bytes).await?;
            state.add_downloaded(bytes.len() as u64);
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&part, dest).await?;
        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::catalog::find_entry;
    use axum::extract::State;
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use std::net::SocketAddr;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mahi-local-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A deterministic pseudo-random blob (so resume corruption is visible).
    fn test_blob(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    /// Serve a blob with `Range: bytes=N-` support (206/416), Ollama-style.
    async fn blob_handler(State(blob): State<Arc<Vec<u8>>>, headers: HeaderMap) -> Response {
        let start = headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.split('-').next())
            .and_then(|v| v.parse::<usize>().ok());
        match start {
            Some(start) if start < blob.len() => (
                StatusCode::PARTIAL_CONTENT,
                [(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, blob.len() - 1, blob.len()),
                )],
                blob[start..].to_vec(),
            )
                .into_response(),
            Some(_) => StatusCode::RANGE_NOT_SATISFIABLE.into_response(),
            None => blob.as_ref().clone().into_response(),
        }
    }

    /// Spawn a local file server for `blob`; returns its base URL.
    async fn spawn_blob_server(blob: Vec<u8>) -> String {
        let app = Router::new()
            .route("/model.gguf", get(blob_handler))
            .with_state(Arc::new(blob));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn local_entry(id: &str, base_url: &str, size: u64) -> CatalogEntry {
        CatalogEntry {
            id: id.to_string(),
            display_name: "Test Model".to_string(),
            family: "test".to_string(),
            size_bytes: size,
            quantization: "Q4_K_M".to_string(),
            context_window: 4096,
            tool_calling: false,
            download_url: format!("{base_url}/model.gguf"),
            file_name: "model.gguf".to_string(),
            description: "test".to_string(),
        }
    }

    #[test]
    fn part_path_appends_suffix() {
        let dest = PathBuf::from("/models/llama-3.2-1b-instruct.gguf");
        assert_eq!(
            part_path(&dest),
            PathBuf::from("/models/llama-3.2-1b-instruct.gguf.part")
        );
    }

    #[test]
    fn content_range_total_parses() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_RANGE,
            "bytes 1000-65535/65536".parse().unwrap(),
        );
        assert_eq!(content_range_total(&headers), Some(65536));
        headers.clear();
        assert_eq!(content_range_total(&headers), None);
        headers.insert(reqwest::header::CONTENT_RANGE, "garbage".parse().unwrap());
        assert_eq!(content_range_total(&headers), None);
    }

    #[test]
    fn download_state_machine_transitions() {
        let state = DownloadState::default();
        assert_eq!(state.status(), DownloadStatus::Idle);
        assert_eq!(state.bytes_downloaded(), 0);
        assert_eq!(state.progress(), 0.0);
        assert!(!state.is_cancelled());

        state.set_status(DownloadStatus::Downloading);
        state.set_total(100);
        state.add_downloaded(25);
        state.add_downloaded(25);
        assert_eq!(state.status(), DownloadStatus::Downloading);
        assert_eq!(state.bytes_downloaded(), 50);
        assert_eq!(state.total_bytes(), 100);
        assert!((state.progress() - 0.5).abs() < f32::EPSILON);

        state.set_status(DownloadStatus::Completed);
        assert_eq!(state.status(), DownloadStatus::Completed);

        state.set_status(DownloadStatus::Failed("boom".into()));
        assert_eq!(state.status(), DownloadStatus::Failed("boom".into()));

        state.cancel();
        assert!(state.is_cancelled());
    }

    #[test]
    fn model_path_is_id_dot_gguf() {
        let mgr = ModelManager::new("/models");
        assert_eq!(
            mgr.model_path("qwen2.5-0.5b-instruct"),
            PathBuf::from("/models/qwen2.5-0.5b-instruct.gguf")
        );
        // Dotted ids keep their full stem.
        assert_eq!(
            mgr.model_path("mistral-7b-instruct-v0.3"),
            PathBuf::from("/models/mistral-7b-instruct-v0.3.gguf")
        );
    }

    #[tokio::test]
    async fn installed_scans_and_matches_catalog() {
        let dir = temp_dir("installed");
        std::fs::write(dir.join("qwen2.5-0.5b-instruct.gguf"), b"fake-gguf").unwrap();
        std::fs::write(dir.join("my-custom-model.gguf"), b"custom").unwrap();
        std::fs::write(dir.join("incomplete.gguf.part"), b"partial").unwrap();
        std::fs::write(dir.join("notes.txt"), b"ignore me").unwrap();

        let mgr = ModelManager::new(&dir);
        let installed = mgr.installed().await;
        assert_eq!(installed.len(), 2);

        let qwen = installed
            .iter()
            .find(|m| m.id == "qwen2.5-0.5b-instruct")
            .expect("catalog model found");
        assert!(qwen.catalog_entry.is_some());
        assert_eq!(qwen.size_bytes, 9);

        let custom = installed
            .iter()
            .find(|m| m.id == "my-custom-model")
            .expect("arbitrary gguf found");
        assert!(custom.catalog_entry.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn installed_on_missing_dir_is_empty() {
        let mgr = ModelManager::new("/definitely/not/a/real/dir/mahi");
        assert!(mgr.installed().await.is_empty());
    }

    #[tokio::test]
    async fn download_streams_part_then_renames() {
        let blob = test_blob(64 * 1024);
        let base = spawn_blob_server(blob.clone()).await;
        let dir = temp_dir("download");
        let mgr = ModelManager::new(&dir);
        let entry = local_entry("test-model", &base, blob.len() as u64);

        let handle = mgr.start_download(&entry).await;
        let state = handle.state();
        let status = handle.wait().await;
        assert_eq!(status, DownloadStatus::Completed);
        assert_eq!(state.bytes_downloaded(), blob.len() as u64);
        assert_eq!(state.total_bytes(), blob.len() as u64);
        assert!((state.progress() - 1.0).abs() < f32::EPSILON);

        let dest = mgr.model_path("test-model");
        assert_eq!(std::fs::read(&dest).unwrap(), blob);
        assert!(!part_path(&dest).exists(), ".part must be renamed away");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Resume must append from the `.part` offset (proved by seeding the
    /// `.part` with bytes that differ from the blob's prefix).
    #[tokio::test]
    async fn download_resumes_from_existing_part() {
        let blob = test_blob(64 * 1024);
        let base = spawn_blob_server(blob.clone()).await;
        let dir = temp_dir("resume");
        let dest = dir.join("test-model.gguf");
        let seeded = vec![0xABu8; 1000];
        std::fs::write(part_path(&dest), &seeded).unwrap();

        let state = DownloadState::default();
        let client = reqwest::Client::new();
        let url = format!("{base}/model.gguf");
        download_with_resume(&client, &url, &dest, &state)
            .await
            .expect("resume succeeds");

        let result = std::fs::read(&dest).unwrap();
        assert_eq!(result.len(), blob.len());
        assert_eq!(&result[..1000], &seeded[..], "seeded prefix kept (append)");
        assert_eq!(
            &result[1000..],
            &blob[1000..],
            "remainder fetched via Range"
        );
        assert_eq!(state.total_bytes(), blob.len() as u64);
        assert_eq!(state.bytes_downloaded(), blob.len() as u64);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An oversized stale `.part` triggers HTTP 416; the manager clears it
    /// and restarts from scratch.
    #[tokio::test]
    async fn download_recovers_from_unsatisfiable_range() {
        let blob = test_blob(8 * 1024);
        let base = spawn_blob_server(blob.clone()).await;
        let dir = temp_dir("range416");
        let dest = dir.join("test-model.gguf");
        std::fs::write(part_path(&dest), vec![0u8; blob.len() + 5000]).unwrap();

        let state = DownloadState::default();
        let client = reqwest::Client::new();
        let url = format!("{base}/model.gguf");
        download_with_resume(&client, &url, &dest, &state)
            .await
            .expect("recovers via full restart");
        assert_eq!(std::fs::read(&dest).unwrap(), blob);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn cancelled_download_keeps_part_and_reports_failed() {
        let blob = test_blob(64 * 1024);
        let base = spawn_blob_server(blob.clone()).await;
        let dir = temp_dir("cancel");
        let mgr = ModelManager::new(&dir);
        let entry = local_entry("test-model", &base, blob.len() as u64);

        let handle = mgr.start_download(&entry).await;
        handle.cancel(); // request cancellation immediately
        let status = handle.wait().await;
        assert_eq!(status, DownloadStatus::Failed("download cancelled".into()));
        assert!(
            !mgr.model_path("test-model").exists(),
            "no complete file after cancel"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn download_of_already_installed_model_completes_instantly() {
        let dir = temp_dir("already");
        let mgr = ModelManager::new(&dir);
        let dest = mgr.model_path("test-model");
        std::fs::write(&dest, b"already here").unwrap();

        // URL is unreachable on purpose: it must not be contacted.
        let entry = local_entry("test-model", "http://127.0.0.1:9", 12);
        let status = mgr.start_download(&entry).await.wait().await;
        assert_eq!(status, DownloadStatus::Completed);
        assert_eq!(std::fs::read(&dest).unwrap(), b"already here");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn download_http_error_surfaces_as_failed() {
        let blob = test_blob(128);
        let base = spawn_blob_server(blob).await;
        let dir = temp_dir("httperr");
        let mgr = ModelManager::new(&dir);
        let mut entry = local_entry("test-model", &base, 128);
        entry.download_url = format!("{base}/missing.gguf"); // 404

        let status = mgr.start_download(&entry).await.wait().await;
        match status {
            DownloadStatus::Failed(msg) => assert!(msg.contains("404"), "got: {msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn delete_removes_model_and_part() {
        let dir = temp_dir("delete");
        let mgr = ModelManager::new(&dir);
        let dest = mgr.model_path("doomed");
        std::fs::write(&dest, b"bytes").unwrap();
        std::fs::write(part_path(&dest), b"partial").unwrap();

        mgr.delete("doomed").await.expect("delete succeeds");
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());

        match mgr.delete("doomed").await {
            Err(LocalError::ModelNotFound(id)) => assert_eq!(id, "doomed"),
            other => panic!("expected ModelNotFound, got {other:?}"),
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Real-network smoke test (HF must be reachable); run with `--ignored`.
    #[tokio::test]
    #[ignore = "hits huggingface.co; verifies the smallest catalog URL is live"]
    async fn catalog_smallest_model_url_is_live() {
        let entry = find_entry("qwen2.5-0.5b-instruct").unwrap();
        let client = reqwest::Client::new();
        let resp = client
            .get(&entry.download_url)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send()
            .await
            .expect("HF reachable");
        assert!(
            resp.status().is_success(),
            "unexpected status {}",
            resp.status()
        );
    }
}
