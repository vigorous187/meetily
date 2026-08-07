use crate::export_core::sanitize_markdown_name;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

fn default_export_directory() -> Result<PathBuf, String> {
    dirs::download_dir()
        .or_else(dirs::document_dir)
        .ok_or_else(|| "Could not find a Downloads or Documents directory".to_string())
}

fn approved_export_roots() -> Vec<PathBuf> {
    [dirs::download_dir(), dirs::document_dir()]
        .into_iter()
        .flatten()
        .filter_map(|path| path.canonicalize().ok())
        .collect()
}

fn validated_export_directory(directory: &Path) -> Result<PathBuf, String> {
    let directory = directory
        .canonicalize()
        .map_err(|_| "Export folder must already exist".to_string())?;
    let approved = approved_export_roots();
    if approved.iter().any(|root| directory.starts_with(root)) {
        Ok(directory)
    } else {
        Err("Exports are limited to your Downloads or Documents folder".to_string())
    }
}

fn create_export_file(directory: &Path, file_stem: &str, content: &str) -> Result<PathBuf, String> {
    // create_new is atomic: an existing file or symlink is never followed or
    // overwritten, including if another process races this export.
    for suffix in 0..10_000_u32 {
        let file_name = if suffix == 0 {
            format!("{file_stem}.md")
        } else {
            format!("{file_stem}-{suffix}.md")
        };
        let path = directory.join(file_name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(content.as_bytes()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("Failed to export meeting: {error}"));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Failed to export meeting: {error}")),
        }
    }
    Err("Could not choose a unique export filename".to_string())
}

#[tauri::command]
pub fn export_meeting_markdown(
    directory_path: Option<String>,
    suggested_name: String,
    content: String,
) -> Result<String, String> {
    if content.trim().is_empty() {
        return Err("Cannot export an empty meeting".to_string());
    }

    let requested_directory = match directory_path.filter(|path| !path.trim().is_empty()) {
        Some(path) => PathBuf::from(path),
        None => default_export_directory()?,
    };
    let directory = validated_export_directory(&requested_directory)?;

    let file_stem = sanitize_markdown_name(&suggested_name);
    let export_path = create_export_file(&directory, &file_stem, &content)?;
    Ok(export_path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::create_export_file;

    #[test]
    fn export_never_overwrites_existing_file() {
        let directory =
            std::env::temp_dir().join(format!("meetily-secure-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("notes.md"), "original").unwrap();

        let path = create_export_file(&directory, "notes", "new").unwrap();
        assert_eq!(path.file_name().unwrap(), "notes-1.md");
        assert_eq!(
            std::fs::read_to_string(directory.join("notes.md")).unwrap(),
            "original"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn export_does_not_follow_filename_symlink() {
        use std::os::unix::fs::symlink;

        let directory = std::env::temp_dir().join(format!(
            "meetily-secure-export-symlink-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let target = directory.join("target.txt");
        std::fs::write(&target, "private").unwrap();
        symlink(&target, directory.join("notes.md")).unwrap();

        let path = create_export_file(&directory, "notes", "new").unwrap();
        assert_eq!(path.file_name().unwrap(), "notes-1.md");
        assert_eq!(std::fs::read_to_string(target).unwrap(), "private");

        std::fs::remove_dir_all(directory).unwrap();
    }
}
