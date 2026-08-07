use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    path::Path,
    process::Command,
};

struct ReviewedSidecar {
    name: &'static str,
    size: u64,
    sha256: &'static str,
}

const APPLE_SILICON_SIDECARS: [ReviewedSidecar; 2] = [
    ReviewedSidecar {
        name: "llama-helper",
        size: 5_190_784,
        sha256: "68a72d9a4edf64c8284f79e6379e2f0dad5b2d94118591b025f070c8e5fa0daf",
    },
    ReviewedSidecar {
        name: "diarization-helper",
        size: 23_505_600,
        sha256: "78ec589bdd38c8d041d6cf5c49c852022c6d996bdf10ef106bb8376040038001",
    },
];

/// Validate all native sidecars before Tauri copies them into the target and
/// application bundle. Nothing is downloaded and unreviewed target artifacts
/// fail the build.
pub fn ensure_reviewed_sidecars() {
    let target = std::env::var("TARGET")
        .or_else(|_| std::env::var("HOST"))
        .expect("Neither TARGET nor HOST environment variable set");
    if target != "aarch64-apple-darwin" {
        panic!(
            "No reviewed llama-helper and diarization-helper artifacts are registered for target {target}"
        );
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR environment variable not set");
    let binary_dir = Path::new(&manifest_dir).join("binaries");
    println!("cargo:rerun-if-changed=binaries/SIDECAR-PROVENANCE.md");

    for expected in &APPLE_SILICON_SIDECARS {
        let binary_path = binary_dir.join(format!("{}-{target}", expected.name));
        println!("cargo:rerun-if-changed={}", binary_path.display());
        verify_sidecar(&binary_path, expected).unwrap_or_else(|error| {
            panic!(
                "Unsafe {} artifact {}: {error}",
                expected.name,
                binary_path.display()
            )
        });
        println!("cargo:warning=Reviewed {} artifact verified", expected.name);
    }
}

fn verify_sidecar(path: &Path, expected: &ReviewedSidecar) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect artifact: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("symlinks are forbidden for packaged external binaries".to_string());
    }
    if !metadata.file_type().is_file() {
        return Err("artifact is not a regular file".to_string());
    }
    if metadata.len() != expected.size {
        return Err(format!(
            "size mismatch: found {}, expected {}",
            metadata.len(),
            expected.size
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("artifact is not executable".to_string());
        }
    }

    let actual_hash = sha256_file(path)?;
    if actual_hash != expected.sha256 {
        return Err(format!(
            "SHA-256 mismatch: found {actual_hash}, expected {}",
            expected.sha256
        ));
    }

    verify_apple_silicon_binary(path)
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| format!("could not open artifact: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("could not hash artifact: {error}"))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_apple_silicon_binary(path: &Path) -> Result<(), String> {
    let architectures = tool_output("/usr/bin/lipo", &[OsStr::new("-archs"), path.as_os_str()])?;
    if architectures.trim() != "arm64" {
        return Err(format!(
            "architecture mismatch: found {}, expected arm64",
            architectures.trim()
        ));
    }

    let build_version = tool_output(
        "/usr/bin/vtool",
        &[OsStr::new("-show-build"), path.as_os_str()],
    )?;
    if !build_version
        .lines()
        .any(|line| line.trim() == "minos 11.0")
    {
        return Err("minimum macOS version is not exactly 11.0".to_string());
    }

    let linked_libraries = tool_output("/usr/bin/otool", &[OsStr::new("-L"), path.as_os_str()])?;
    for line in linked_libraries.lines().skip(1) {
        let Some(library) = line.split_whitespace().next() else {
            continue;
        };
        if !library.starts_with("/usr/lib/") && !library.starts_with("/System/Library/") {
            return Err(format!(
                "non-system dynamic dependency is forbidden: {library}"
            ));
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewed_records_are_unique_and_well_formed() {
        assert_ne!(
            APPLE_SILICON_SIDECARS[0].name,
            APPLE_SILICON_SIDECARS[1].name
        );
        for record in &APPLE_SILICON_SIDECARS {
            assert!(record.size > 0);
            assert_eq!(record.sha256.len(), 64);
            assert!(record.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn reviewed_repository_inputs_pass_verification() {
        let binary_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries");
        for record in &APPLE_SILICON_SIDECARS {
            let path = binary_dir.join(format!("{}-aarch64-apple-darwin", record.name));
            verify_sidecar(&path, record).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_non_executable_inputs_before_tooling() {
        use std::os::unix::fs::symlink;

        let directory = std::env::temp_dir().join(format!(
            "meetily-sidecar-verifier-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let target = directory.join("target");
        let link = directory.join("link");
        File::create(&target).unwrap();
        symlink(&target, &link).unwrap();
        let empty = ReviewedSidecar {
            name: "test",
            size: 0,
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        };

        assert!(verify_sidecar(&link, &empty)
            .unwrap_err()
            .contains("symlinks are forbidden"));
        assert!(verify_sidecar(&target, &empty)
            .unwrap_err()
            .contains("not executable"));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
