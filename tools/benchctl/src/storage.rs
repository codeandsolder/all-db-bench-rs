use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Default)]
struct Counts {
    apparent_bytes: u64,
    allocated_block_bytes: u64,
    files: u64,
    directories: u64,
    symlinks: u64,
}

fn walk(root: &Path, counts: &mut Counts) -> Result<()> {
    let entries = fs::read_dir(root)
        .with_context(|| format!("failed to read directory {}", root.display()))?;
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_symlink() {
            counts.symlinks = counts.symlinks.saturating_add(1);
        } else if file_type.is_dir() {
            counts.directories = counts.directories.saturating_add(1);
            if let Err(error) = walk(&path, counts) {
                eprintln!("warning: skipping {}: {error:#}", path.display());
            }
        } else if file_type.is_file() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            counts.files = counts.files.saturating_add(1);
            counts.apparent_bytes = counts.apparent_bytes.saturating_add(metadata.len());
            counts.allocated_block_bytes = counts
                .allocated_block_bytes
                .saturating_add(metadata.blocks().saturating_mul(512));
        }
    }
    Ok(())
}

fn command_text(program: &str, args: &[&str]) -> String {
    match Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }
        _ => String::new(),
    }
}

fn zfs_value(source: &str, property: &str) -> Value {
    let raw = command_text("zfs", &["get", "-Hp", "-o", "value", property, source]);
    match raw.parse::<u64>() {
        Ok(value) => json!(value),
        Err(_) => json!(raw),
    }
}

pub(crate) fn footprint(root: &Path) -> Result<Value> {
    if !root.exists() {
        bail!("path does not exist: {}", root.display());
    }
    let root: PathBuf = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;
    let mut counts = Counts::default();
    walk(&root, &mut counts)?;

    let root_text = root.to_string_lossy();
    let source = command_text("findmnt", &["-T", &root_text, "-n", "-o", "SOURCE"]);
    let filesystem = command_text("findmnt", &["-T", &root_text, "-n", "-o", "FSTYPE"]);
    let target = command_text("findmnt", &["-T", &root_text, "-n", "-o", "TARGET"]);

    let reliable = filesystem != "zfs";
    let mut payload = Map::from_iter([
        ("path".to_owned(), json!(root)),
        ("filesystem".to_owned(), json!(filesystem)),
        ("source".to_owned(), json!(source)),
        ("mount_target".to_owned(), json!(target)),
        ("apparent_bytes".to_owned(), json!(counts.apparent_bytes)),
        (
            "allocated_block_bytes".to_owned(),
            json!(counts.allocated_block_bytes),
        ),
        ("allocated_block_bytes_reliable".to_owned(), json!(reliable)),
        (
            "allocated_block_bytes_semantics".to_owned(),
            json!(if reliable {
                "stat.st_blocks*512 filesystem block accounting"
            } else {
                "stat.st_blocks*512; not a physical-allocation measure on ZFS"
            }),
        ),
        ("file_count".to_owned(), json!(counts.files)),
        ("directory_count".to_owned(), json!(counts.directories)),
        ("symlink_count".to_owned(), json!(counts.symlinks)),
    ]);

    if filesystem == "zfs" && !source.is_empty() {
        let mut zfs = Map::new();
        for property in [
            "compression",
            "compressratio",
            "recordsize",
            "primarycache",
            "secondarycache",
            "sync",
            "logbias",
            "used",
            "logicalused",
            "referenced",
            "logicalreferenced",
        ] {
            zfs.insert(property.to_owned(), zfs_value(&source, property));
        }
        payload.insert("zfs_dataset".to_owned(), Value::Object(zfs));
    }

    Ok(Value::Object(payload))
}

pub(crate) fn print_footprint(root: &Path) -> Result<()> {
    println!("{}", serde_json::to_string(&footprint(root)?)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::footprint;
    use anyhow::Result;
    use std::fs;

    #[test]
    fn counts_apparent_bytes_files_directories_and_symlinks() -> Result<()> {
        let temp = tempfile_dir()?;
        fs::create_dir(temp.join("sub"))?;
        fs::write(temp.join("a"), b"abcd")?;
        fs::write(temp.join("sub").join("b"), b"123456")?;
        std::os::unix::fs::symlink(temp.join("a"), temp.join("file-link"))?;
        std::os::unix::fs::symlink(temp.join("sub"), temp.join("dir-link"))?;

        let value = footprint(&temp)?;
        assert_eq!(value["apparent_bytes"].as_u64(), Some(10));
        assert_eq!(value["file_count"].as_u64(), Some(2));
        assert_eq!(value["directory_count"].as_u64(), Some(1));
        assert_eq!(value["symlink_count"].as_u64(), Some(2));
        fs::remove_dir_all(temp)?;
        Ok(())
    }

    fn tempfile_dir() -> Result<std::path::PathBuf> {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "all-db-benchctl-storage-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        fs::create_dir(&path)?;
        Ok(path)
    }
}
