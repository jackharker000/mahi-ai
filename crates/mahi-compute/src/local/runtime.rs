//! Managed `llama-server` runtime.
//!
//! The app owns the whole lifecycle so the user never installs anything: on
//! first use [`LlamaRuntime::ensure_binary`] downloads a pinned prebuilt
//! `llama-server` from the ggml-org/llama.cpp GitHub releases (the slice
//! matching the running CPU architecture), unzips it next to its dylibs, and
//! [`LlamaRuntime::start`] runs it as a `kill_on_drop` child process on a free
//! loopback port, health-checked before it is handed back. The OpenAI-compat
//! HTTP server it exposes is what [`crate::local::LocalLlamaProvider`] talks to.

use crate::local::manager::{download_with_resume, DownloadState};
use crate::local::LocalError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::{Child, Command};

/// Pinned llama.cpp release tag.
///
/// This is the **last** release that ships prebuilt macOS `llama-server`
/// binaries for *both* Apple Silicon (`-bin-macos-arm64.zip`) and Intel
/// (`-bin-macos-x64.zip`) — later tags dropped the macOS server zips. Both
/// asset URLs were verified to return HTTP 200, and the build is recent enough
/// (mid-2025) to support every model in [`crate::local::model_catalog`] with
/// `--jinja` chat templates and OpenAI-style tool calling.
pub const LLAMA_CPP_TAG: &str = "b6000";

/// Default server context window when a model doesn't override it.
pub const DEFAULT_CONTEXT_SIZE: u32 = 8192;

/// How long to wait for a freshly spawned server to report healthy.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(120);

/// The llama.cpp asset architecture token for the running CPU slice.
///
/// Evaluated per compiled slice, so each half of the universal app binary
/// resolves to its own architecture at runtime.
fn llama_asset_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        "arm64"
    }
}

/// The GitHub release download URL for `arch` (`"arm64"` / `"x64"`) at the
/// pinned tag.
fn asset_url(arch: &str) -> String {
    format!(
        "https://github.com/ggml-org/llama.cpp/releases/download/{tag}/llama-{tag}-bin-macos-{arch}.zip",
        tag = LLAMA_CPP_TAG,
    )
}

/// Bind an ephemeral loopback port, read it, and release it. The small race
/// between releasing and the server re-binding is acceptable on `127.0.0.1`.
pub fn pick_free_port() -> Result<u16, LocalError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    Ok(port)
}

/// Coarse runtime state for the UI / FFI status surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RuntimeStatus {
    /// No `llama-server` binary installed yet.
    NotInstalled,
    /// Downloading/installing the `llama-server` binary.
    Downloading,
    /// Binary installed, no model loaded.
    Ready,
    /// A model is loading into a starting server.
    Starting,
    /// A server is up and serving `model_id` on `port`.
    Running { model_id: String, port: u16 },
    /// The runtime failed; carries a human-readable reason.
    Failed(String),
}

/// Manages the `llama-server` binary and the child process running a model.
pub struct LlamaRuntime {
    runtime_dir: PathBuf,
}

impl LlamaRuntime {
    /// A runtime backed by `runtime_dir` (where the server binary + dylibs
    /// live). The directory is created on demand.
    pub fn new(runtime_dir: impl Into<PathBuf>) -> Self {
        Self {
            runtime_dir: runtime_dir.into(),
        }
    }

    /// The directory holding the server binary and its dylibs.
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Where the `llama-server` executable is (or will be) installed.
    pub fn server_binary_path(&self) -> PathBuf {
        self.runtime_dir.join("llama-server")
    }

    /// Whether the server binary is already installed.
    pub fn is_installed(&self) -> bool {
        self.server_binary_path().is_file()
    }

    /// Ensure the `llama-server` binary exists, downloading + unpacking the
    /// pinned prebuilt for this architecture if needed. Returns its path.
    pub async fn ensure_binary(&self) -> Result<PathBuf, LocalError> {
        let binary = self.server_binary_path();
        if binary.is_file() {
            return Ok(binary);
        }
        tokio::fs::create_dir_all(&self.runtime_dir).await?;

        // Download the release zip (atomic rename via the shared downloader).
        let url = asset_url(llama_asset_arch());
        let zip = self.runtime_dir.join("llama-server.zip");
        let _ = tokio::fs::remove_file(&zip).await;
        let client = reqwest::Client::new();
        let state = DownloadState::default();
        download_with_resume(&client, &url, &zip, &state).await?;

        // Unzip into a scratch dir, then flatten the binary + its dylibs into
        // runtime_dir so @loader_path/@rpath references resolve.
        let extract = self.runtime_dir.join("unzip");
        let _ = tokio::fs::remove_dir_all(&extract).await;
        tokio::fs::create_dir_all(&extract).await?;
        run_unzip(&zip, &extract).await?;

        let server_src = find_file_named(&extract, "llama-server")
            .ok_or_else(|| LocalError::Runtime("llama-server not found in release zip".into()))?;
        let runtime_payload_dir = server_src
            .parent()
            .ok_or_else(|| LocalError::Runtime("invalid extracted layout".into()))?;
        copy_runtime_files(runtime_payload_dir, &self.runtime_dir)?;

        // Best-effort cleanup of the scratch artifacts.
        let _ = tokio::fs::remove_dir_all(&extract).await;
        let _ = tokio::fs::remove_file(&zip).await;

        make_executable(&binary)?;
        if !binary.is_file() {
            return Err(LocalError::Runtime(
                "llama-server missing after install".into(),
            ));
        }
        Ok(binary)
    }

    /// Start `llama-server` for `model_path` and wait until it reports healthy.
    ///
    /// `--jinja` enables the model's real chat template and OpenAI-compatible
    /// tool calling. The returned [`RunningServer`] kills the child on drop.
    pub async fn start(
        &self,
        model_path: &Path,
        ctx_size: u32,
    ) -> Result<RunningServer, LocalError> {
        if !model_path.is_file() {
            return Err(LocalError::ModelNotFound(model_path.display().to_string()));
        }
        let binary = self.ensure_binary().await?;
        let port = pick_free_port()?;
        let model_id = model_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("local")
            .to_string();

        let mut child = Command::new(&binary)
            .arg("-m")
            .arg(model_path)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("-c")
            .arg(ctx_size.max(512).to_string())
            .arg("--jinja")
            .current_dir(&self.runtime_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| LocalError::Runtime(format!("failed to spawn llama-server: {e}")))?;

        match wait_until_healthy(&mut child, port, HEALTH_TIMEOUT).await {
            Ok(()) => Ok(RunningServer {
                child,
                port,
                model_id,
            }),
            Err(err) => {
                let _ = child.kill().await;
                Err(err)
            }
        }
    }
}

/// A running `llama-server` child serving one model on a loopback port.
pub struct RunningServer {
    child: Child,
    port: u16,
    model_id: String,
}

impl RunningServer {
    /// The loopback base URL of the OpenAI-compatible server.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The bound loopback port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The file-stem id of the loaded model.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// This server's [`RuntimeStatus::Running`].
    pub fn status(&self) -> RuntimeStatus {
        RuntimeStatus::Running {
            model_id: self.model_id.clone(),
            port: self.port,
        }
    }

    /// Stop the server (also happens on drop via `kill_on_drop`).
    pub async fn stop(mut self) {
        let _ = self.child.kill().await;
    }
}

/// Poll `http://127.0.0.1:<port>/health` until it returns 200, the child
/// exits, or the timeout elapses.
async fn wait_until_healthy(
    child: &mut Child,
    port: u16,
    timeout: Duration,
) -> Result<(), LocalError> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/health");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // If the server already exited, fail fast with its status.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(LocalError::Runtime(format!(
                "llama-server exited before becoming healthy ({status})"
            )));
        }
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(LocalError::Runtime(format!(
                "llama-server did not become healthy within {}s",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Run `/usr/bin/unzip -o <zip> -d <dest>`.
async fn run_unzip(zip: &Path, dest: &Path) -> Result<(), LocalError> {
    let status = Command::new("/usr/bin/unzip")
        .arg("-o")
        .arg("-q")
        .arg(zip)
        .arg("-d")
        .arg(dest)
        .status()
        .await
        .map_err(|e| LocalError::Runtime(format!("failed to run unzip: {e}")))?;
    if !status.success() {
        return Err(LocalError::Runtime(format!("unzip failed ({status})")));
    }
    Ok(())
}

/// Depth-first search for a regular file named exactly `name` under `root`.
fn find_file_named(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
                return Some(path);
            }
        }
    }
    None
}

/// Copy the server binary and every co-located runtime file (`.dylib`,
/// `.metal`, plus the binary itself) from `src_dir` into `dest_dir`, flat.
fn copy_runtime_files(src_dir: &Path, dest_dir: &Path) -> Result<(), LocalError> {
    for entry in std::fs::read_dir(src_dir)?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let keep = match path.extension().and_then(|e| e.to_str()) {
            Some("dylib") | Some("metal") => true,
            _ => path.file_name().and_then(|n| n.to_str()) == Some("llama-server"),
        };
        if !keep {
            continue;
        }
        if let Some(name) = path.file_name() {
            std::fs::copy(&path, dest_dir.join(name))?;
        }
    }
    Ok(())
}

/// `chmod u+rwx,go+rx` on a file (Unix only; a no-op elsewhere).
fn make_executable(path: &Path) -> Result<(), LocalError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.is_file() {
            let mut perms = std::fs::metadata(path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms)?;
        }
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_tag_and_asset_urls_are_well_formed() {
        assert_eq!(LLAMA_CPP_TAG, "b6000");
        let arm = asset_url("arm64");
        let x64 = asset_url("x64");
        assert_eq!(
            arm,
            "https://github.com/ggml-org/llama.cpp/releases/download/b6000/llama-b6000-bin-macos-arm64.zip"
        );
        assert!(x64.ends_with("llama-b6000-bin-macos-x64.zip"));
    }

    #[test]
    fn asset_arch_is_one_of_the_two_macos_slices() {
        assert!(matches!(llama_asset_arch(), "arm64" | "x64"));
    }

    #[test]
    fn free_port_is_nonzero_and_actually_bindable() {
        let port = pick_free_port().expect("a free port");
        assert_ne!(port, 0);
        // We can bind it again after release.
        std::net::TcpListener::bind(("127.0.0.1", port)).expect("rebind released port");
    }

    #[test]
    fn binary_path_is_under_runtime_dir() {
        let rt = LlamaRuntime::new("/opt/mahi/runtime");
        assert_eq!(
            rt.server_binary_path(),
            PathBuf::from("/opt/mahi/runtime/llama-server")
        );
        assert!(!rt.is_installed());
    }

    #[test]
    fn running_status_reports_model_and_port() {
        let status = RuntimeStatus::Running {
            model_id: "qwen2.5-7b-instruct".into(),
            port: 1234,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["state"], "running");
        assert_eq!(json["model_id"], "qwen2.5-7b-instruct");
        assert_eq!(json["port"], 1234);
    }

    #[test]
    fn find_file_named_walks_recursively() {
        let dir = std::env::temp_dir().join(format!("mahi-rt-{}", uuid::Uuid::new_v4().simple()));
        let nested = dir.join("build").join("bin");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("llama-server"), b"#!/bin/sh\n").unwrap();
        std::fs::write(nested.join("libllama.dylib"), b"x").unwrap();

        let found = find_file_named(&dir, "llama-server").expect("found nested binary");
        assert_eq!(found, nested.join("llama-server"));
        assert!(find_file_named(&dir, "does-not-exist").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copy_runtime_files_keeps_binary_and_libs_only() {
        let src =
            std::env::temp_dir().join(format!("mahi-rt-src-{}", uuid::Uuid::new_v4().simple()));
        let dest =
            std::env::temp_dir().join(format!("mahi-rt-dst-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(src.join("llama-server"), b"bin").unwrap();
        std::fs::write(src.join("libllama.dylib"), b"lib").unwrap();
        std::fs::write(src.join("ggml.metal"), b"metal").unwrap();
        std::fs::write(src.join("README.md"), b"docs").unwrap();
        std::fs::write(src.join("llama-cli"), b"other").unwrap();

        copy_runtime_files(&src, &dest).unwrap();
        assert!(dest.join("llama-server").is_file());
        assert!(dest.join("libllama.dylib").is_file());
        assert!(dest.join("ggml.metal").is_file());
        // Docs and unrelated binaries are not copied.
        assert!(!dest.join("README.md").exists());
        assert!(!dest.join("llama-cli").exists());

        std::fs::remove_dir_all(&src).ok();
        std::fs::remove_dir_all(&dest).ok();
    }
}
