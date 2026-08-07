use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

const HASH_BUFFER_SIZE: usize = 1024 * 1024;

pub async fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).await.with_context(|| {
        format!(
            "Failed to open {} for integrity verification",
            path.display()
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_BUFFER_SIZE];

    loop {
        let bytes_read = file.read(&mut buffer).await.with_context(|| {
            format!(
                "Failed to read {} for integrity verification",
                path.display()
            )
        })?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

pub async fn verify_sha256(path: &Path, expected_sha256: &str) -> Result<()> {
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(anyhow!(
            "Invalid trusted SHA-256 value configured for {}",
            path.display()
        ));
    }

    let actual_sha256 = sha256_file(path).await?;
    if !actual_sha256.eq_ignore_ascii_case(expected_sha256) {
        return Err(anyhow!(
            "SHA-256 mismatch for {} (expected {}, got {})",
            path.display(),
            expected_sha256.to_ascii_lowercase(),
            actual_sha256
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_implementation_matches_known_vector() {
        let mut hasher = Sha256::new();
        hasher.update(b"abc");
        assert_eq!(
            format!("{:x}", hasher.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
