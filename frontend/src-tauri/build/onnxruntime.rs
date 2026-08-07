use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

const TARGET: &str = "aarch64-apple-darwin";
const ARCHIVE_SIZE: u64 = 71_060_144;
const ARCHIVE_SHA256: &str = "e5c83560aa9e88afa39d9dca9fb5f5a767e28adb5458d1c36fe0357131b6af8b";

/// Verify the exact static ONNX Runtime archive selected for ort-sys.
/// The archive is a reviewed local build input; this function never downloads.
pub fn ensure_onnxruntime_archive() {
    let target = std::env::var("TARGET")
        .or_else(|_| std::env::var("HOST"))
        .expect("Neither TARGET nor HOST environment variable set");
    if target != TARGET {
        panic!("No reviewed ONNX Runtime archive is registered for target {target}");
    }

    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR environment variable not set"),
    );
    let reviewed_root = manifest_dir.join("vendor/onnxruntime").join(TARGET);
    let archive = reviewed_root.join("lib/libonnxruntime.a");

    let configured_root = std::env::var_os("ORT_LIB_LOCATION")
        .map(PathBuf::from)
        .expect("ORT_LIB_LOCATION must select the reviewed static ONNX Runtime archive");
    let configured_root = configured_root
        .canonicalize()
        .expect("ORT_LIB_LOCATION does not resolve to a readable directory");
    let reviewed_root = reviewed_root
        .canonicalize()
        .expect("reviewed ONNX Runtime directory is unavailable");
    if configured_root != reviewed_root {
        panic!("ORT_LIB_LOCATION does not select the reviewed ONNX Runtime directory");
    }
    if std::env::var("ORT_SKIP_DOWNLOAD").as_deref() != Ok("true") {
        panic!("ORT_SKIP_DOWNLOAD must remain true for reproducible offline builds");
    }

    println!("cargo:rerun-if-changed={}", archive.display());
    println!("cargo:rerun-if-changed=vendor/onnxruntime/PROVENANCE.md");
    println!("cargo:rerun-if-changed=vendor/onnxruntime/LICENSE");
    verify_archive(&archive).unwrap_or_else(|error| {
        panic!(
            "Unsafe ONNX Runtime artifact {}: {error}",
            archive.display()
        )
    });
    println!("cargo:warning=Reviewed static ONNX Runtime 1.22.0 artifact verified");
}

fn verify_archive(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect archive: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("symlinks are forbidden for reviewed native archives".to_string());
    }
    if !metadata.file_type().is_file() {
        return Err("archive is not a regular file".to_string());
    }
    if metadata.len() != ARCHIVE_SIZE {
        return Err(format!(
            "size mismatch: found {}, expected {ARCHIVE_SIZE}",
            metadata.len()
        ));
    }
    let actual_hash = sha256_file(path)?;
    if actual_hash != ARCHIVE_SHA256 {
        return Err(format!(
            "SHA-256 mismatch: found {actual_hash}, expected {ARCHIVE_SHA256}"
        ));
    }

    let architectures = tool_output("/usr/bin/lipo", &[OsStr::new("-archs"), path.as_os_str()])?;
    if architectures.trim() != "arm64" {
        return Err(format!(
            "architecture mismatch: found {}, expected arm64",
            architectures.trim()
        ));
    }

    let symbols = tool_output("/usr/bin/nm", &[OsStr::new("-gU"), path.as_os_str()])?;
    if !symbols
        .lines()
        .any(|line| line.split_whitespace().last() == Some("_OrtGetApiBase"))
    {
        return Err("archive does not export OrtGetApiBase".to_string());
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| format!("could not open archive: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("could not hash archive: {error}"))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn tool_output(tool: &str, arguments: &[&OsStr]) -> Result<String, String> {
    let output = Command::new(tool)
        .args(arguments)
        .output()
        .map_err(|error| format!("could not run {tool}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{tool} exited unsuccessfully"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
