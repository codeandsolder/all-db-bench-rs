//! Run directory and result publication shared by the benchmark lanes.
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Component, Path, PathBuf},
};

/// Prepare exactly one named database directory under the caller's root.
///
/// # Errors
/// Rejects non-normal names, non-directory entries (including symlinks),
/// missing databases in reuse mode, and filesystem errors.
pub fn prepare_database(root: &Path, name: &str, reuse: bool) -> Result<PathBuf> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(part)) if part == name)
        || components.next().is_some()
    {
        bail!("database name must be one plain path component");
    }
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(meta) if !meta.is_dir() => {
            bail!("database path is not a plain directory: {}", path.display())
        }
        Ok(_) if reuse => return Ok(path),
        Ok(_) => fs::remove_dir_all(&path)
            .with_context(|| format!("remove stale database {}", path.display()))?,
        Err(error) if error.kind() == ErrorKind::NotFound && reuse => bail!(
            "--reuse-db requested but database path does not exist: {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspect database {}", path.display()));
        }
    }
    fs::create_dir_all(root)?;
    fs::create_dir(&path).with_context(|| format!("create database {}", path.display()))?;
    Ok(path)
}

#[derive(Clone, Copy)]
pub enum ReportMode {
    AppendLine,
    ReplacePretty,
}

fn publish(result: &impl Serialize, output: &str, mode: ReportMode) -> Result<()> {
    let json = match mode {
        ReportMode::AppendLine => serde_json::to_string(result)?,
        ReportMode::ReplacePretty => serde_json::to_string_pretty(result)?,
    };
    if output == "-" {
        println!("{json}");
    } else {
        match mode {
            ReportMode::AppendLine => {
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(output)?;
                writeln!(file, "{json}")?;
            }
            ReportMode::ReplacePretty => {
                if let Some(parent) = Path::new(output).parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(output, format!("{json}\n"))?;
            }
        }
    }
    Ok(())
}

/// Publish the finished measurement before optionally removing the database.
///
/// # Errors
/// Returns serialization or filesystem failures; retains the database if
/// publication fails.
pub fn finish_database(
    path: &Path,
    keep: bool,
    result: &impl Serialize,
    output: &str,
    mode: ReportMode,
) -> Result<()> {
    publish(result, output, mode)?;
    if !keep {
        fs::remove_dir_all(path)
            .with_context(|| format!("remove completed database {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ReportMode, finish_database, prepare_database};
    use anyhow::Result;
    use serde_json::json;
    use std::{fs, os::unix::fs::symlink, path::PathBuf};

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Result<Self> {
            let dir = std::env::temp_dir()
                .join(format!("all-db-bench-db-directory-{}", std::process::id()));
            fs::create_dir(&dir)?;
            Ok(Self(dir))
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn directory_lifecycle_never_resolves_an_escaped_name() -> Result<()> {
        let dir = TestDir::new()?;
        let root = dir.0.join("root");
        let outside = dir.0.join("outside");
        fs::create_dir_all(&outside)?;
        fs::write(outside.join("keep"), b"sentinel")?;
        for name in [
            "",
            "/",
            ".",
            "./",
            "..",
            "../outside",
            "./fresh",
            "fresh/",
            "a/b",
        ] {
            assert!(prepare_database(&root, name, false).is_err(), "{name}");
        }
        assert!(prepare_database(&root, "missing", true).is_err());
        let path = prepare_database(&root, "database", false)?;
        fs::write(path.join("old"), b"old")?;
        assert_eq!(prepare_database(&root, "database", true)?, path);
        assert!(path.join("old").exists());
        assert_eq!(prepare_database(&root, "database", false)?, path);
        assert!(!path.join("old").exists());
        symlink(&outside, root.join("link"))?;
        for reuse in [false, true] {
            assert!(prepare_database(&root, "link", reuse).is_err());
        }
        assert!(outside.join("keep").is_file());
        // Output failure retains evidence. Successful append writes before cleanup.
        assert!(
            finish_database(
                &path,
                false,
                &json!({"ops": 1}),
                &root.display().to_string(),
                ReportMode::AppendLine
            )
            .is_err()
        );
        assert!(path.is_dir());
        let out = dir.0.join("results.ndjson");
        let out_str = out.display().to_string();
        finish_database(
            &path,
            true,
            &json!({"ops": 1}),
            &out_str,
            ReportMode::AppendLine,
        )?;
        finish_database(
            &path,
            false,
            &json!({"ops": 2}),
            &out_str,
            ReportMode::AppendLine,
        )?;
        assert_eq!(fs::read_to_string(out)?, "{\"ops\":1}\n{\"ops\":2}\n");
        assert!(!path.exists());
        Ok(())
    }
}
