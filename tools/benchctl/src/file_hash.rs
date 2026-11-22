use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::Path};

pub fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

pub fn sha256(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(lower_hex(&digest.finalize()))
}

/// Digest the complete noise source manifest, matching scripts/noise-source-sha256.sh.
pub fn noise_source_sha256(repo: &Path) -> Result<String> {
    let dir = Path::new("tools/benchctl/src/noise");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(repo.join(dir))? {
        let entry = entry?;
        let name = entry.file_name();
        if Path::new(&name).extension().is_some_and(|ext| ext == "rs") {
            files.push(dir.join(name));
        }
    }
    files.sort_unstable();
    files.insert(0, "tools/benchctl/src/noise.rs".into());
    let mut digest = Sha256::new();
    for file in files {
        let hash = sha256(&repo.join(&file))?;
        digest.update(format!("{hash}  {}\n", file.display()).as_bytes());
    }
    Ok(lower_hex(&digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::noise_source_sha256;
    use anyhow::{Context, Result};
    use std::{path::Path, process::Command};

    #[test]
    fn shell_and_rust_noise_source_hashes_agree() -> Result<()> {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?;
        let output = Command::new(repo.join("scripts/noise-source-sha256.sh"))
            .arg(&repo)
            .output()?;
        assert!(output.status.success());
        assert_eq!(
            std::str::from_utf8(&output.stdout)?.trim(),
            noise_source_sha256(&repo)?
        );
        assert_eq!(noise_source_sha256(&repo)?.len(), 64);
        let old =
            super::sha256(&repo.join("tools/benchctl/src/noise.rs")).context("old source hash")?;
        assert_ne!(old, noise_source_sha256(&repo)?);
        Ok(())
    }
}
