// Sidecar process lifecycle management for llama-helper
// Handles spawning, health checking, keep-alive, and graceful shutdown

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, RwLock};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

use super::models;

#[cfg(all(not(debug_assertions), target_os = "macos", target_arch = "aarch64"))]
const PACKAGED_LLAMA_HELPER_SIZE: u64 = 5_190_784;
#[cfg(all(not(debug_assertions), target_os = "macos", target_arch = "aarch64"))]
const PACKAGED_LLAMA_HELPER_SHA256: &str =
    "68a72d9a4edf64c8284f79e6379e2f0dad5b2d94118591b025f070c8e5fa0daf";

fn file_matches(path: &Path, expected_size: u64, expected_sha256: &str) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() != expected_size
    {
        return false;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let Ok(count) = file.read(&mut buffer) else {
            return false;
        };
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    format!("{:x}", hasher.finalize()) == expected_sha256
}

fn verify_helper_before_spawn(path: &Path) -> Result<()> {
    #[cfg(all(not(debug_assertions), target_os = "macos", target_arch = "aarch64"))]
    if !file_matches(
        path,
        PACKAGED_LLAMA_HELPER_SIZE,
        PACKAGED_LLAMA_HELPER_SHA256,
    ) {
        return Err(anyhow!(
            "bundled llama-helper failed packaged size or SHA-256 verification"
        ));
    }

    #[cfg(all(not(debug_assertions), not(all(target_os = "macos", target_arch = "aarch64"))))]
    return Err(anyhow!(
        "bundled llama-helper has no pinned runtime verification record for this platform"
    ));

    #[cfg(debug_assertions)]
    let _ = path;
    Ok(())
}

fn adjacent_regular_executable(executable: &Path, binary_name: &str) -> Option<PathBuf> {
    let executable = executable.canonicalize().ok()?;
    let executable_dir = executable.parent()?.canonicalize().ok()?;
    let candidate = executable_dir.join(binary_name);
    let metadata = std::fs::symlink_metadata(&candidate).ok()?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    let candidate = candidate.canonicalize().ok()?;
    if candidate.parent()?.canonicalize().ok()? != executable_dir {
        return None;
    }
    Some(candidate)
}

#[cfg(debug_assertions)]
fn developer_executable(path: PathBuf) -> Option<PathBuf> {
    // Developer overrides may legitimately be symlinks. Release resolution
    // uses the stricter adjacent_regular_executable path above.
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    path.canonicalize().ok()
}

#[cfg(debug_assertions)]
fn target_triple() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return "x86_64-unknown-linux-gnu";
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return "aarch64-unknown-linux-gnu";
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return "x86_64-apple-darwin";
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return "aarch64-apple-darwin";
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return "x86_64-pc-windows-msvc";
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    return "aarch64-pc-windows-msvc";
    #[allow(unreachable_code)]
    "unknown"
}

// ============================================================================
// Sidecar State Management
// ============================================================================

/// Sidecar process manager with keep-alive and health monitoring
pub struct SidecarManager {
    /// Child process handle
    child_process: Arc<Mutex<Option<Child>>>,

    /// Stdin writer for sending requests
    stdin_writer: Arc<Mutex<Option<ChildStdin>>>,

    /// Stdout reader for receiving responses
    stdout_reader: Arc<Mutex<Option<BufReader<ChildStdout>>>>,

    /// Last activity timestamp
    last_activity: Arc<RwLock<Instant>>,

    /// Health status
    is_healthy: Arc<AtomicBool>,

    /// Shutdown flag
    should_shutdown: Arc<AtomicBool>,

    /// Active request count (for graceful shutdown)
    active_request_count: Arc<AtomicUsize>,

    /// Path to llama-helper binary
    helper_binary_path: PathBuf,

    /// Current model path (if loaded)
    current_model_path: Arc<RwLock<Option<PathBuf>>>,

    /// Idle timeout in seconds (configurable via env var)
    idle_timeout_secs: u64,
}

/// RAII guard for tracking active requests
/// Decrements the active request count when dropped
struct RequestGuard {
    counter: Arc<AtomicUsize>,
}

impl RequestGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self { counter }
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

impl SidecarManager {
    /// Create a new sidecar manager
    pub fn new(_app_data_dir: PathBuf) -> Result<Self> {
        let helper_binary_path = Self::resolve_helper_binary()?;

        // Get idle timeout from env var or use default
        let idle_timeout_secs = std::env::var("LLAMA_IDLE_TIMEOUT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(models::DEFAULT_IDLE_TIMEOUT_SECS);

        log::info!(
            "SidecarManager initialized with idle timeout: {}s",
            idle_timeout_secs
        );

        Ok(Self {
            child_process: Arc::new(Mutex::new(None)),
            stdin_writer: Arc::new(Mutex::new(None)),
            stdout_reader: Arc::new(Mutex::new(None)),
            last_activity: Arc::new(RwLock::new(Instant::now())),
            is_healthy: Arc::new(AtomicBool::new(false)),
            should_shutdown: Arc::new(AtomicBool::new(false)),
            active_request_count: Arc::new(AtomicUsize::new(0)),
            helper_binary_path,
            current_model_path: Arc::new(RwLock::new(None)),
            idle_timeout_secs,
        })
    }

    /// Resolve the path to llama-helper binary.
    ///
    /// Tauri strips the target-triple suffix when it bundles an external binary,
    /// so a release accepts exactly `llama-helper[.exe]` beside the current
    /// executable. Environment, workspace, resource-directory, and fuzzy-name
    /// fallbacks are developer conveniences and are not compiled into releases.
    fn resolve_helper_binary() -> Result<PathBuf> {
        let executable = std::env::current_exe().context("Failed to locate current executable")?;
        let binary_name = if cfg!(windows) {
            "llama-helper.exe"
        } else {
            "llama-helper"
        };

        if let Some(path) = adjacent_regular_executable(&executable, binary_name) {
            log::info!("Using bundled llama-helper sidecar");
            return Ok(path);
        }

        #[cfg(not(debug_assertions))]
        return Err(anyhow!(
            "bundled llama-helper sidecar is missing or failed validation"
        ));

        #[cfg(debug_assertions)]
        {
            if let Some(path) = std::env::var_os("MEETILY_LLAMA_HELPER")
                .filter(|value| !value.is_empty())
                .and_then(|value| developer_executable(PathBuf::from(value)))
            {
                log::info!("Using developer llama-helper override");
                return Ok(path);
            }

            let executable_dir = executable
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let target_name = format!(
                "llama-helper-{}{}",
                target_triple(),
                if cfg!(windows) { ".exe" } else { "" }
            );
            if let Some(path) = developer_executable(executable_dir.join(target_name)) {
                log::info!("Using target-specific developer llama-helper");
                return Ok(path);
            }

            if let Some(resource_dir) = std::env::var_os("RESOURCE_DIR") {
                let resource_dir = PathBuf::from(resource_dir);
                let target_name = format!(
                    "llama-helper-{}{}",
                    target_triple(),
                    if cfg!(windows) { ".exe" } else { "" }
                );
                for candidate in [
                    resource_dir.join(binary_name),
                    resource_dir.join(target_name),
                ] {
                    if let Some(path) = developer_executable(candidate) {
                        log::info!("Using developer llama-helper resource");
                        return Ok(path);
                    }
                }
            }

            if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
                let manifest_dir = PathBuf::from(manifest_dir);
                if let Some(project_root) = manifest_dir.parent().and_then(|path| path.parent()) {
                    for candidate in [
                        project_root.join("target/release/llama-helper"),
                        project_root.join("target/debug/llama-helper"),
                        project_root.join("target/release/llama-helper.exe"),
                        project_root.join("target/debug/llama-helper.exe"),
                    ] {
                        if let Some(path) = developer_executable(candidate) {
                            log::info!("Using workspace developer llama-helper");
                            return Ok(path);
                        }
                    }
                }
            }

            Err(anyhow!(
                "llama-helper binary not found; build it or set MEETILY_LLAMA_HELPER in a debug build"
            ))
        }
    }

    /// Ensure sidecar is running, spawn if needed
    pub async fn ensure_running(&self, model_path: PathBuf) -> Result<()> {
        // Check if already running with correct model
        {
            let current_model = self.current_model_path.read().await;
            if current_model.as_ref() == Some(&model_path) && self.is_healthy() {
                log::debug!("Sidecar already running with correct model");
                self.update_activity().await;
                return Ok(());
            }
        }

        // Need to spawn or restart
        self.spawn(model_path).await
    }

    /// Spawn the sidecar process
    async fn spawn(&self, model_path: PathBuf) -> Result<()> {
        // Shutdown existing process if running
        self.shutdown().await?;

        log::info!("Spawning llama-helper sidecar");
        verify_helper_before_spawn(&self.helper_binary_path)?;

        #[cfg(unix)]
        let mut command = tokio::process::Command::new("/usr/bin/nice");

        #[cfg(not(unix))]
        let mut command = tokio::process::Command::new(&self.helper_binary_path);

        #[cfg(unix)]
        command.arg("-n").arg("10").arg(&self.helper_binary_path);

        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // Log stderr to main process
            .env("LLAMA_IDLE_TIMEOUT", self.idle_timeout_secs.to_string());

        #[cfg(target_os = "windows")]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x00004000;

            command.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
        }

        let mut child = command
            .spawn()
            .context("Failed to spawn bundled llama-helper")?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdout"))?;

        // Store handles
        {
            let mut child_lock = self.child_process.lock().await;
            *child_lock = Some(child);
        }

        {
            let mut stdin_lock = self.stdin_writer.lock().await;
            *stdin_lock = Some(stdin);
        }

        {
            let mut stdout_lock = self.stdout_reader.lock().await;
            *stdout_lock = Some(BufReader::new(stdout));
        }

        // Update state
        {
            let mut current_model = self.current_model_path.write().await;
            *current_model = Some(model_path);
        }

        self.is_healthy.store(true, Ordering::SeqCst);
        self.should_shutdown.store(false, Ordering::SeqCst);
        self.update_activity().await;

        log::info!("Sidecar spawned successfully");

        // Start background tasks
        self.start_health_check_loop();
        self.start_idle_check_loop();

        Ok(())
    }

    /// Send a request to the sidecar and wait for response
    pub async fn send_request(&self, request_json: String, timeout: Duration) -> Result<String> {
        // Track active request
        let _guard = RequestGuard::new(self.active_request_count.clone());

        // Write request to stdin
        {
            let mut stdin_lock = self.stdin_writer.lock().await;
            let stdin = stdin_lock
                .as_mut()
                .ok_or_else(|| anyhow!("Sidecar not running"))?;

            stdin
                .write_all(request_json.as_bytes())
                .await
                .context("Failed to write request to stdin")?;
            stdin
                .write_all(b"\n")
                .await
                .context("Failed to write newline")?;
            stdin.flush().await.context("Failed to flush stdin")?;
        }

        // Read response from stdout with timeout
        match tokio::time::timeout(timeout, self.read_response()).await {
            Ok(Ok(response)) => {
                self.update_activity().await;
                Ok(response)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // Timeout reached - shutdown sidecar to stop generation
                log::error!("Request timeout after {:?}, shutting down sidecar", timeout);
                if let Err(shutdown_err) = self.shutdown().await {
                    log::error!("Failed to shutdown sidecar after timeout: {}", shutdown_err);
                }
                Err(anyhow!("Request timed out after {:?}", timeout))
            }
        }
    }

    /// Read a single line response from stdout
    async fn read_response(&self) -> Result<String> {
        let mut stdout_lock = self.stdout_reader.lock().await;
        let reader = stdout_lock
            .as_mut()
            .ok_or_else(|| anyhow!("Sidecar not running"))?;

        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .context("Failed to read response from stdout")?;

        if line.is_empty() {
            return Err(anyhow!("Sidecar closed stdout (process may have crashed)"));
        }

        Ok(line.trim().to_string())
    }

    /// Send ping to keep sidecar alive
    async fn send_ping(&self) -> Result<()> {
        let request = serde_json::json!({"type": "ping"}).to_string();
        let timeout = Duration::from_secs(5);

        // Note: We don't use send_request here to avoid incrementing active_request_count
        // for internal health checks, as that would prevent graceful shutdown

        // Write request
        {
            let mut stdin_lock = self.stdin_writer.lock().await;
            if let Some(stdin) = stdin_lock.as_mut() {
                stdin.write_all(request.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;
            } else {
                return Err(anyhow!("Sidecar not running"));
            }
        }

        // Read response
        let response = tokio::time::timeout(timeout, self.read_response()).await??;

        let resp: serde_json::Value = serde_json::from_str(&response)?;
        if resp.get("type").and_then(|t| t.as_str()) == Some("pong") {
            Ok(())
        } else {
            Err(anyhow!("Unexpected ping response: {}", response))
        }
    }

    /// Gracefully shutdown the sidecar
    /// Waits for active requests to complete before killing the process
    pub async fn shutdown_gracefully(&self) -> Result<()> {
        log::info!("Initiating graceful shutdown of sidecar");

        // Set shutdown flag to prevent new internal tasks
        self.should_shutdown.store(true, Ordering::SeqCst);

        // Wait for active requests to complete
        // We poll every 500ms
        let start = Instant::now();
        let max_wait = Duration::from_secs(600); // Wait up to 10 minutes for long generations

        loop {
            let count = self.active_request_count.load(Ordering::SeqCst);
            if count == 0 {
                log::info!("No active requests, proceeding with shutdown");
                break;
            }

            if start.elapsed() > max_wait {
                log::warn!(
                    "Timed out waiting for active requests ({} active), forcing shutdown",
                    count
                );
                break;
            }

            log::debug!("Waiting for {} active requests to complete...", count);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        self.shutdown().await
    }

    /// Force shutdown the sidecar
    pub async fn shutdown(&self) -> Result<()> {
        // Set shutdown flag
        self.should_shutdown.store(true, Ordering::SeqCst);

        // Send shutdown command
        if self.is_healthy() {
            let request = serde_json::json!({"type": "shutdown"}).to_string();
            let _timeout = Duration::from_secs(5);

            // Try to send shutdown command, but ignore errors
            // We don't use send_request to avoid incrementing counter
            let _ = async {
                let mut stdin_lock = self.stdin_writer.lock().await;
                if let Some(stdin) = stdin_lock.as_mut() {
                    stdin.write_all(request.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
        }

        // Kill process if still running
        {
            let mut child_lock = self.child_process.lock().await;
            if let Some(mut child) = child_lock.take() {
                match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                    Ok(Ok(status)) => {
                        log::info!("Sidecar exited with status: {}", status);
                    }
                    Ok(Err(e)) => {
                        log::error!("Failed to wait for sidecar: {}", e);
                    }
                    Err(_) => {
                        log::warn!("Sidecar didn't exit gracefully, killing");
                        let _ = child.kill().await;
                    }
                }
            }
        }

        // Clear handles
        {
            let mut stdin_lock = self.stdin_writer.lock().await;
            *stdin_lock = None;
        }

        {
            let mut stdout_lock = self.stdout_reader.lock().await;
            *stdout_lock = None;
        }

        {
            let mut current_model = self.current_model_path.write().await;
            *current_model = None;
        }

        self.is_healthy.store(false, Ordering::SeqCst);

        log::info!("Sidecar shutdown complete");
        Ok(())
    }

    /// Check if sidecar is healthy
    pub fn is_healthy(&self) -> bool {
        self.is_healthy.load(Ordering::SeqCst)
    }

    /// Update last activity timestamp
    async fn update_activity(&self) {
        let mut last_activity = self.last_activity.write().await;
        *last_activity = Instant::now();
    }

    /// Get seconds since last activity
    async fn seconds_since_activity(&self) -> u64 {
        let last_activity = self.last_activity.read().await;
        last_activity.elapsed().as_secs()
    }

    /// Start health check loop (runs in background)
    fn start_health_check_loop(&self) {
        let manager = Self {
            child_process: self.child_process.clone(),
            stdin_writer: self.stdin_writer.clone(),
            stdout_reader: self.stdout_reader.clone(),
            last_activity: self.last_activity.clone(),
            is_healthy: self.is_healthy.clone(),
            should_shutdown: self.should_shutdown.clone(),
            active_request_count: self.active_request_count.clone(),
            helper_binary_path: self.helper_binary_path.clone(),
            current_model_path: self.current_model_path.clone(),
            idle_timeout_secs: self.idle_timeout_secs,
        };

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                if manager.should_shutdown.load(Ordering::SeqCst) {
                    log::debug!("Health check loop: shutdown flag set, exiting");
                    break;
                }

                if !manager.is_healthy() {
                    log::debug!("Health check loop: sidecar unhealthy, skipping ping");
                    continue;
                }

                // Don't ping if we are busy with a request
                if manager.active_request_count.load(Ordering::SeqCst) > 0 {
                    continue;
                }

                log::debug!("Health check: sending ping");
                if let Err(e) = manager.send_ping().await {
                    log::warn!("Health check failed: {}", e);
                    manager.is_healthy.store(false, Ordering::SeqCst);
                }
            }

            log::debug!("Health check loop exited");
        });
    }

    /// Start idle check loop (runs in background)
    fn start_idle_check_loop(&self) {
        let manager = Self {
            child_process: self.child_process.clone(),
            stdin_writer: self.stdin_writer.clone(),
            stdout_reader: self.stdout_reader.clone(),
            last_activity: self.last_activity.clone(),
            is_healthy: self.is_healthy.clone(),
            should_shutdown: self.should_shutdown.clone(),
            active_request_count: self.active_request_count.clone(),
            helper_binary_path: self.helper_binary_path.clone(),
            current_model_path: self.current_model_path.clone(),
            idle_timeout_secs: self.idle_timeout_secs,
        };

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                if manager.should_shutdown.load(Ordering::SeqCst) {
                    log::debug!("Idle check loop: shutdown flag set, exiting");
                    break;
                }

                // Don't shutdown if we are busy
                if manager.active_request_count.load(Ordering::SeqCst) > 0 {
                    // Update activity to prevent timeout immediately after request finishes
                    manager.update_activity().await;
                    continue;
                }

                let idle_secs = manager.seconds_since_activity().await;
                log::debug!("Idle check: {}s since last activity", idle_secs);

                if idle_secs > manager.idle_timeout_secs {
                    log::info!(
                        "Sidecar idle for {}s (timeout: {}s), shutting down",
                        idle_secs,
                        manager.idle_timeout_secs
                    );

                    if let Err(e) = manager.shutdown().await {
                        log::error!("Failed to shutdown idle sidecar: {}", e);
                    }

                    break;
                }
            }

            log::debug!("Idle check loop exited");
        });
    }
}

impl Drop for SidecarManager {
    fn drop(&mut self) {
        // Set shutdown flag
        self.should_shutdown.store(true, Ordering::SeqCst);

        // Note: Actual cleanup happens in shutdown() method
        // We can't do async work in Drop, so this is best-effort
        log::debug!("SidecarManager dropped");
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn exact_hash_verifier_rejects_changed_helper() {
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("helper");
        std::fs::write(&helper, b"reviewed").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"reviewed"));
        assert!(file_matches(&helper, 8, &expected));

        std::fs::write(&helper, b"tampered").unwrap();
        assert!(!file_matches(&helper, 8, &expected));
    }

    #[test]
    fn bundled_helper_must_be_adjacent_regular_executable() {
        let directory = tempfile::tempdir().unwrap();
        let external_directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("meetily");
        let helper = directory.path().join("llama-helper");
        let external_helper = external_directory.path().join("llama-helper");
        File::create(&executable).unwrap();
        File::create(&helper).unwrap();
        File::create(&external_helper).unwrap();
        #[cfg(unix)]
        {
            make_executable(&executable);
            make_executable(&helper);
            make_executable(&external_helper);
        }

        assert_eq!(
            adjacent_regular_executable(&executable, "llama-helper"),
            Some(helper.canonicalize().unwrap())
        );
        assert!(
            adjacent_regular_executable(&executable, external_helper.to_str().unwrap()).is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn bundled_helper_rejects_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("meetily");
        let target = directory.path().join("helper-target");
        let helper = directory.path().join("llama-helper");
        File::create(&executable).unwrap();
        File::create(&target).unwrap();
        make_executable(&executable);
        make_executable(&target);
        symlink(&target, &helper).unwrap();

        assert!(adjacent_regular_executable(&executable, "llama-helper").is_none());
    }
}
