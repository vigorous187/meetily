use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::Path,
    process::Command,
};

const FFMPEG_VERSION: &str = "8.1.2";

struct ReviewedBinary {
    size: u64,
    sha256: &'static str,
}

/// Validate a previously reviewed FFmpeg binary without performing network access.
///
/// Release binaries must be prepared from the signed upstream source by following
/// `binaries/FFMPEG-PROVENANCE.md`. Cargo builds deliberately fail closed if the
/// artifact is missing, is a symlink, or differs from the reviewed artifact.
pub fn ensure_ffmpeg_binary() {
    let target = std::env::var("TARGET")
        .or_else(|_| std::env::var("HOST"))
        .expect("Neither TARGET nor HOST environment variable set");
    let expected = reviewed_binary(&target).unwrap_or_else(|error| panic!("{error}"));
    let extension = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };
    let binary_name = format!("ffmpeg-{target}{extension}");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR environment variable not set");
    let binary_path = Path::new(&manifest_dir).join("binaries").join(&binary_name);

    println!("cargo:rerun-if-changed={}", binary_path.display());
    println!("cargo:rerun-if-changed=binaries/FFMPEG-PROVENANCE.md");
    println!("cargo:rerun-if-changed=binaries/COPYING.LGPLv2.1");
    println!("cargo:warning=Checking reviewed FFmpeg binary for {target}");

    verify_ffmpeg_binary(&binary_path, &target, expected).unwrap_or_else(|error| {
        panic!("Unsafe FFmpeg artifact {}: {error}", binary_path.display())
    });

    println!(
        "cargo:warning=Reviewed FFmpeg {FFMPEG_VERSION} artifact verified: {}",
        binary_path.display()
    );
}

fn reviewed_binary(target: &str) -> Result<ReviewedBinary, String> {
    match target {
        "aarch64-apple-darwin" => Ok(ReviewedBinary {
            size: 22_186_376,
            sha256: "0c6c0dcac32f2b5a9f19e194fb449783f383a9b0051b068342dd38d85198e0a7",
        }),
        _ => Err(format!(
            "No reviewed FFmpeg artifact is registered for target {target}. \
             Build and review one using binaries/FFMPEG-PROVENANCE.md; automatic downloads are disabled."
        )),
    }
}

fn verify_ffmpeg_binary(path: &Path, target: &str, expected: ReviewedBinary) -> Result<(), String> {
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

    verify_version_and_configuration(path)?;
    if target.ends_with("-apple-darwin") {
        verify_macos_binary(path, target)?;
    }
    Ok(())
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

fn verify_version_and_configuration(path: &Path) -> Result<(), String> {
    let version = command_output(path, &["-hide_banner", "-version"])?;
    let expected_version = format!("ffmpeg version {FFMPEG_VERSION}");
    if !version
        .lines()
        .next()
        .is_some_and(|line| line.starts_with(&expected_version))
    {
        return Err(format!("expected {expected_version}"));
    }

    let build_configuration = command_output(path, &["-hide_banner", "-buildconf"])?;
    for required in [
        "--disable-shared",
        "--enable-static",
        "--disable-autodetect",
        "--disable-network",
        "--disable-gpl",
        "--disable-nonfree",
        "--disable-avdevice",
        "-mmacosx-version-min=11.0",
    ] {
        if !build_configuration.contains(required) {
            return Err(format!("missing required build flag {required}"));
        }
    }
    for forbidden in ["--enable-network", "--enable-gpl", "--enable-nonfree"] {
        if build_configuration.contains(forbidden) {
            return Err(format!("forbidden build flag present: {forbidden}"));
        }
    }

    let protocols = command_output(path, &["-hide_banner", "-protocols"])?;
    for forbidden in [
        "http", "https", "tcp", "tls", "udp", "rtmp", "rtsp", "srt", "ssh",
    ] {
        if protocols.lines().any(|line| line.trim() == forbidden) {
            return Err(format!(
                "network protocol is unexpectedly enabled: {forbidden}"
            ));
        }
    }
    Ok(())
}

fn verify_macos_binary(path: &Path, target: &str) -> Result<(), String> {
    let expected_architecture = if target.starts_with("aarch64-") {
        "arm64"
    } else if target.starts_with("x86_64-") {
        "x86_64"
    } else {
        return Err(format!("unsupported macOS architecture in target {target}"));
    };
    let architectures = tool_output("lipo", &[std::ffi::OsStr::new("-archs"), path.as_os_str()])?;
    if architectures.trim() != expected_architecture {
        return Err(format!(
            "architecture mismatch: found {}, expected {expected_architecture}",
            architectures.trim()
        ));
    }

    let build_version = tool_output(
        "vtool",
        &[std::ffi::OsStr::new("-show-build"), path.as_os_str()],
    )?;
    if !build_version
        .lines()
        .any(|line| line.trim() == "minos 11.0")
    {
        return Err("minimum macOS version is not exactly 11.0".to_string());
    }

    let linked_libraries = tool_output("otool", &[std::ffi::OsStr::new("-L"), path.as_os_str()])?;
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

fn command_output(program: &Path, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| format!("could not run {}: {error}", program.display()))?;
    if !output.status.success() {
        return Err(format!("{} exited unsuccessfully", program.display()));
    }
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn tool_output(tool: &str, arguments: &[&std::ffi::OsStr]) -> Result<String, String> {
    let output = Command::new(tool)
        .args(arguments)
        .output()
        .map_err(|error| format!("could not run {tool}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{tool} exited unsuccessfully"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
