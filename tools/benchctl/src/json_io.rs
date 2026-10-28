use anyhow::{Context, Result};
use serde_json::Value;
use std::{fs, path::Path};

pub(crate) fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_str(
        &fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("invalid JSON in {}", path.display()))
}
