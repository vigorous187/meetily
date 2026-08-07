use std::path::{Component, Path, PathBuf};

use tauri::{Manager, Runtime};

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("Could not restrict private meeting storage: {error}"))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

/// Restrict an application-owned meeting directory without following symlinks.
pub fn harden_private_directory(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect private meeting folder: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Private meeting folder must be a regular non-symlink directory".to_string());
    }
    set_mode(path, 0o700)
}

/// Restrict an application-owned meeting file without following symlinks.
pub fn harden_private_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect private meeting file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Private meeting file must be a regular non-symlink file".to_string());
    }
    set_mode(path, 0o600)
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("File path must be absolute".to_string());
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err("File path escapes its root".to_string());
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

fn canonical_roots(roots: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    roots
        .into_iter()
        .filter_map(|root| root.canonicalize().ok())
        .filter(|root| root.is_dir())
        .collect()
}

fn application_roots<R: Runtime>(app: &tauri::AppHandle<R>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(path) = app.path().app_data_dir() {
        roots.push(path);
    }
    roots.extend(
        [
            dirs::video_dir(),
            dirs::document_dir(),
            dirs::download_dir(),
        ]
        .into_iter()
        .flatten(),
    );
    canonical_roots(roots)
}

fn matching_root<'a>(path: &Path, roots: &'a [PathBuf]) -> Option<&'a PathBuf> {
    roots.iter().find(|root| path.starts_with(root))
}

fn ensure_directory_with_roots(path: &Path, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let normalized = normalize_absolute(path)?;
    let root = matching_root(&normalized, roots)
        .ok_or_else(|| "Path is outside approved application folders".to_string())?;
    let relative = normalized
        .strip_prefix(root)
        .map_err(|_| "Path is outside approved application folders".to_string())?;
    let mut current = root.clone();

    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err("Path contains an unsupported component".to_string());
        };
        current.push(value);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(
                        "Approved folder path contains a symlink or non-directory".to_string()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)
                    .map_err(|error| format!("Could not create approved folder: {error}"))?;
            }
            Err(error) => return Err(format!("Could not inspect approved folder: {error}")),
        }
    }

    let canonical = current
        .canonicalize()
        .map_err(|error| format!("Could not verify approved folder: {error}"))?;
    if !canonical.starts_with(root) {
        return Err("Approved folder resolved outside its root".to_string());
    }
    Ok(canonical)
}

fn validate_existing_file_with_roots(path: &Path, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let normalized = normalize_absolute(path)?;
    let link_metadata = std::fs::symlink_metadata(&normalized)
        .map_err(|error| format!("Could not inspect requested file: {error}"))?;
    if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
        return Err("Requested file must be a regular non-symlink file".to_string());
    }
    let canonical = normalized
        .canonicalize()
        .map_err(|error| format!("Could not verify requested file: {error}"))?;
    if matching_root(&canonical, roots).is_none() {
        return Err("Requested file is outside approved application folders".to_string());
    }
    Ok(canonical)
}

fn validate_existing_directory_with_roots(
    path: &Path,
    roots: &[PathBuf],
) -> Result<PathBuf, String> {
    let normalized = normalize_absolute(path)?;
    let root = matching_root(&normalized, roots)
        .ok_or_else(|| "Directory is outside approved application folders".to_string())?;
    let relative = normalized
        .strip_prefix(root)
        .map_err(|_| "Directory is outside approved application folders".to_string())?;
    let mut current = root.clone();

    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err("Directory contains an unsupported component".to_string());
        };
        current.push(value);
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|error| format!("Could not inspect approved directory: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("Approved directory path contains a symlink or non-directory".to_string());
        }
    }

    let canonical = current
        .canonicalize()
        .map_err(|error| format!("Could not verify approved directory: {error}"))?;
    if !canonical.starts_with(root) {
        return Err("Approved directory resolved outside its root".to_string());
    }
    Ok(canonical)
}

fn validate_existing_audio_file_with_roots(
    path: &Path,
    roots: &[PathBuf],
    maximum_bytes: u64,
) -> Result<PathBuf, String> {
    const AUDIO_EXTENSIONS: &[&str] = &[
        "aac", "flac", "m4a", "mkv", "mp3", "mp4", "ogg", "wav", "webm", "wma",
    ];
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| "Audio file extension is missing".to_string())?;
    if !AUDIO_EXTENSIONS.contains(&extension.as_str()) {
        return Err("Requested file type is not an approved audio format".to_string());
    }
    let canonical = validate_existing_file_with_roots(path, roots)?;
    let size = std::fs::metadata(&canonical)
        .map_err(|error| format!("Could not inspect requested audio file: {error}"))?
        .len();
    if size > maximum_bytes {
        return Err(format!(
            "Requested audio file exceeds the {} MiB playback limit",
            maximum_bytes / (1024 * 1024)
        ));
    }
    Ok(canonical)
}

fn validate_new_file_with_roots(path: &Path, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let normalized = normalize_absolute(path)?;
    let file_name = normalized
        .file_name()
        .ok_or_else(|| "Output file name is missing".to_string())?
        .to_owned();
    let parent = normalized
        .parent()
        .ok_or_else(|| "Output folder is missing".to_string())?;
    let parent = ensure_directory_with_roots(parent, roots)?;
    let output = parent.join(file_name);
    match std::fs::symlink_metadata(&output) {
        Ok(_) => Err("Output file already exists".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(output),
        Err(error) => Err(format!("Could not inspect output file: {error}")),
    }
}

pub fn ensure_approved_directory<R: Runtime>(
    app: &tauri::AppHandle<R>,
    path: &Path,
) -> Result<PathBuf, String> {
    ensure_directory_with_roots(path, &application_roots(app))
}

pub fn validate_existing_approved_directory<R: Runtime>(
    app: &tauri::AppHandle<R>,
    path: &Path,
) -> Result<PathBuf, String> {
    validate_existing_directory_with_roots(path, &application_roots(app))
}

pub fn validate_existing_app_audio_file<R: Runtime>(
    app: &tauri::AppHandle<R>,
    path: &Path,
) -> Result<PathBuf, String> {
    const MAXIMUM_AUDIO_BYTES: u64 = 512 * 1024 * 1024;
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Could not resolve application data folder: {error}"))?;
    let roots = canonical_roots([app_data]);
    if roots.is_empty() {
        return Err("Application data folder is unavailable".to_string());
    }
    validate_existing_audio_file_with_roots(path, &roots, MAXIMUM_AUDIO_BYTES)
}

pub fn validate_new_file<R: Runtime>(
    app: &tauri::AppHandle<R>,
    path: &Path,
) -> Result<PathBuf, String> {
    validate_new_file_with_roots(path, &application_roots(app))
}

/// Repair permissions for a meeting directory already referenced by the local
/// database. The directory must be below (not equal to) an approved root. Only
/// direct meeting files and the app-owned checkpoint directory are changed;
/// symlinks and unrelated nested directories are never followed.
pub fn harden_existing_meeting_storage<R: Runtime>(
    app: &tauri::AppHandle<R>,
    path: &Path,
) -> Result<(), String> {
    let roots = application_roots(app);
    let directory = validate_existing_directory_with_roots(path, &roots)?;
    let root = matching_root(&directory, &roots)
        .ok_or_else(|| "Meeting folder is outside approved application folders".to_string())?;
    if &directory == root {
        return Err("Refusing to change permissions on an approved root folder".to_string());
    }

    harden_private_directory(&directory)?;
    for entry in std::fs::read_dir(&directory)
        .map_err(|error| format!("Could not inspect existing meeting storage: {error}"))?
    {
        let entry = entry
            .map_err(|error| format!("Could not inspect existing meeting entry: {error}"))?;
        let metadata = entry
            .file_type()
            .map_err(|error| format!("Could not inspect existing meeting entry type: {error}"))?;
        if metadata.is_symlink() {
            continue;
        }
        if metadata.is_file() {
            harden_private_file(&entry.path())?;
            continue;
        }
        if metadata.is_dir() && entry.file_name() == ".checkpoints" {
            let checkpoints = entry.path();
            harden_private_directory(&checkpoints)?;
            for checkpoint in std::fs::read_dir(&checkpoints).map_err(|error| {
                format!("Could not inspect existing meeting checkpoints: {error}")
            })? {
                let checkpoint = checkpoint.map_err(|error| {
                    format!("Could not inspect existing meeting checkpoint: {error}")
                })?;
                let checkpoint_type = checkpoint.file_type().map_err(|error| {
                    format!("Could not inspect existing meeting checkpoint type: {error}")
                })?;
                if checkpoint_type.is_file() && !checkpoint_type.is_symlink() {
                    harden_private_file(&checkpoint.path())?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn private_storage_permissions_are_repaired() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("meeting");
        std::fs::create_dir(&directory).unwrap();
        let file = directory.join("transcripts.json");
        std::fs::write(&file, b"private").unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

        harden_private_directory(&directory).unwrap();
        harden_private_file(&file).unwrap();

        assert_eq!(
            std::fs::metadata(&directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn creates_only_beneath_approved_root() {
        let temporary = tempfile::tempdir().unwrap();
        let temporary_root = temporary.path().canonicalize().unwrap();
        let root = temporary_root.join("approved");
        std::fs::create_dir(&root).unwrap();
        let roots = canonical_roots([root.clone()]);

        let created = ensure_directory_with_roots(&root.join("one/two"), &roots).unwrap();
        assert!(created.ends_with("approved/one/two"));
        assert!(ensure_directory_with_roots(&temporary_root.join("outside"), &roots).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape_and_symlink_file() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let temporary_root = temporary.path().canonicalize().unwrap();
        let root = temporary_root.join("approved");
        let outside = temporary_root.join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), "secret").unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        symlink(outside.join("secret"), root.join("secret-link")).unwrap();
        let roots = canonical_roots([root.clone()]);

        assert!(ensure_directory_with_roots(&root.join("escape/new"), &roots).is_err());
        assert!(validate_existing_file_with_roots(&root.join("secret-link"), &roots).is_err());
        assert!(validate_new_file_with_roots(&root.join("escape/output"), &roots).is_err());
    }

    #[test]
    fn new_file_must_not_replace_existing_content() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap().join("approved");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("notes.md"), "original").unwrap();
        let roots = canonical_roots([root.clone()]);

        assert!(validate_new_file_with_roots(&root.join("notes.md"), &roots).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("notes.md")).unwrap(),
            "original"
        );
    }

    #[test]
    fn audio_files_require_an_approved_extension_and_sane_size() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap().join("approved");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("audio.mp4"), b"audio").unwrap();
        std::fs::write(root.join("audio.txt"), b"audio").unwrap();
        let roots = canonical_roots([root.clone()]);

        assert!(
            validate_existing_audio_file_with_roots(&root.join("audio.mp4"), &roots, 5).is_ok()
        );
        assert!(
            validate_existing_audio_file_with_roots(&root.join("audio.txt"), &roots, 5).is_err()
        );
        assert!(
            validate_existing_audio_file_with_roots(&root.join("audio.mp4"), &roots, 4).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_directories_reject_symlink_components() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path().canonicalize().unwrap();
        let root = base.join("approved");
        let outside = base.join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        let roots = canonical_roots([root.clone()]);

        assert!(validate_existing_directory_with_roots(&root, &roots).is_ok());
        assert!(validate_existing_directory_with_roots(&root.join("escape"), &roots).is_err());
    }
}
