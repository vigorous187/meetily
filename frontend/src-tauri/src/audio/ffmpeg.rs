use log::{debug, error};
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(debug_assertions)]
use which::which;

#[cfg(not(windows))]
const EXECUTABLE_NAME: &str = "ffmpeg";

#[cfg(windows)]
const EXECUTABLE_NAME: &str = "ffmpeg.exe";

static FFMPEG_PATH: Lazy<Option<PathBuf>> = Lazy::new(find_ffmpeg_path_internal);

#[cfg(all(not(debug_assertions), target_os = "macos", target_arch = "aarch64"))]
const PACKAGED_FFMPEG_SIZE: u64 = 22_186_376;
#[cfg(all(not(debug_assertions), target_os = "macos", target_arch = "aarch64"))]
const PACKAGED_FFMPEG_SHA256: &str =
    "0c6c0dcac32f2b5a9f19e194fb449783f383a9b0051b068342dd38d85198e0a7";

pub fn find_ffmpeg_path() -> Option<PathBuf> {
    let path = FFMPEG_PATH.as_ref()?.clone();
    if verify_ffmpeg_before_spawn(&path) {
        Some(path)
    } else {
        error!("Bundled ffmpeg failed packaged size or SHA-256 verification");
        None
    }
}

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

fn verify_ffmpeg_before_spawn(path: &Path) -> bool {
    #[cfg(all(not(debug_assertions), target_os = "macos", target_arch = "aarch64"))]
    return file_matches(path, PACKAGED_FFMPEG_SIZE, PACKAGED_FFMPEG_SHA256);

    #[cfg(all(not(debug_assertions), not(all(target_os = "macos", target_arch = "aarch64"))))]
    return false;

    #[cfg(debug_assertions)]
    {
        let _ = path;
        true
    }
}

/// Resolve a bundled sidecar only when it is an executable regular file located
/// directly beside the application executable. This intentionally rejects
/// symlinks and path traversal so release builds cannot be redirected to an
/// untrusted ffmpeg installation.
fn adjacent_bundled_binary(executable: &Path, binary_name: &str) -> Option<PathBuf> {
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
fn debug_executable(path: &Path) -> Option<PathBuf> {
    // Developer PATH entries are commonly symlinks (for example Homebrew).
    // Following them is acceptable here because this code is absent in release.
    let metadata = std::fs::metadata(path).ok()?;
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

fn find_ffmpeg_path_internal() -> Option<PathBuf> {
    let current_exe = std::env::current_exe().ok();
    if let Some(path) = current_exe
        .as_deref()
        .and_then(|exe| adjacent_bundled_binary(exe, EXECUTABLE_NAME))
    {
        debug!("Using bundled ffmpeg sidecar");
        return Some(path);
    }

    // Release builds fail closed. They never consult PATH, HOME, the current
    // directory, environment overrides, or the network for executable code.
    #[cfg(not(debug_assertions))]
    {
        error!("Bundled ffmpeg sidecar is missing or failed validation");
        None
    }

    // Developer builds retain convenient local fallbacks, but still require a
    // regular executable file. These paths are not compiled into releases.
    #[cfg(debug_assertions)]
    {
        if let Some(path) = std::env::var_os("MEETILY_FFMPEG")
            .filter(|value| !value.is_empty())
            .and_then(|value| debug_executable(Path::new(&value)))
        {
            debug!("Using developer ffmpeg override");
            return Some(path);
        }

        if let Ok(path) = which(EXECUTABLE_NAME) {
            if let Some(path) = debug_executable(&path) {
                debug!("Using ffmpeg from developer PATH");
                return Some(path);
            }
        }

        #[cfg(target_os = "macos")]
        if let Some(path) = dirs::home_dir()
            .map(|home| home.join(".local/bin").join(EXECUTABLE_NAME))
            .and_then(|path| debug_executable(&path))
        {
            debug!("Using developer ffmpeg from .local/bin");
            return Some(path);
        }

        if let Some(path) = std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join(EXECUTABLE_NAME))
            .and_then(|path| debug_executable(&path))
        {
            debug!("Using developer ffmpeg from current directory");
            return Some(path);
        }

        #[cfg(target_os = "macos")]
        if let Some(path) = current_exe
            .as_deref()
            .and_then(Path::parent)
            .map(|dir| dir.join("../Resources").join(EXECUTABLE_NAME))
            .and_then(|path| debug_executable(&path))
        {
            debug!("Using developer ffmpeg from Resources");
            return Some(path);
        }

        error!("ffmpeg executable not found; release bundles must include the verified sidecar");
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn exact_hash_verifier_rejects_changed_ffmpeg() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("ffmpeg");
        std::fs::write(&binary, b"reviewed").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"reviewed"));
        assert!(file_matches(&binary, 8, &expected));

        std::fs::write(&binary, b"tampered").unwrap();
        assert!(!file_matches(&binary, 8, &expected));
    }

    #[test]
    fn accepts_only_adjacent_regular_executable() {
        let directory = tempfile::tempdir().unwrap();
        let external_directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("meetily");
        let ffmpeg = directory.path().join(EXECUTABLE_NAME);
        let external_ffmpeg = external_directory.path().join(EXECUTABLE_NAME);
        File::create(&executable).unwrap();
        File::create(&ffmpeg).unwrap();
        File::create(&external_ffmpeg).unwrap();
        #[cfg(unix)]
        {
            make_executable(&executable);
            make_executable(&ffmpeg);
            make_executable(&external_ffmpeg);
        }

        assert_eq!(
            adjacent_bundled_binary(&executable, EXECUTABLE_NAME),
            Some(ffmpeg.canonicalize().unwrap())
        );
        assert!(adjacent_bundled_binary(&executable, external_ffmpeg.to_str().unwrap()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_non_executable_sidecars() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("meetily");
        let target = directory.path().join("real-ffmpeg");
        let sidecar = directory.path().join(EXECUTABLE_NAME);
        File::create(&executable).unwrap();
        File::create(&target).unwrap();
        make_executable(&executable);

        assert!(adjacent_bundled_binary(&executable, EXECUTABLE_NAME).is_none());
        make_executable(&target);
        symlink(&target, &sidecar).unwrap();
        assert!(adjacent_bundled_binary(&executable, EXECUTABLE_NAME).is_none());
    }
}
