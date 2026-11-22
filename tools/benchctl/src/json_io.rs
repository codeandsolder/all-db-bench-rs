use anyhow::{Context, Result};
use serde_json::Value;
use std::{fs, path::Path};

pub fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_str(
        &fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("invalid JSON in {}", path.display()))
}

pub fn read_ndjson(path: &Path) -> Result<Vec<Value>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("invalid JSON at {}:{}", path.display(), index + 1))
        })
        .collect()
}
