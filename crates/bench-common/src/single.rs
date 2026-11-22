//! Common single-client command options and database evidence.
use crate::{
    measurement::{SetupTimings, WorkMeasurement, dir_size, peak_rss_kib},
    recovery::RecoveryVerification,
    run_directory::{ReportMode, finish_database, prepare_database},
};
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, clap::Args)]
pub struct SingleOptions {
    #[arg(long, default_value = "data")]
    pub root: PathBuf,
    #[arg(long, default_value = "-")]
    pub output: String,
    #[arg(long, default_value_t = false)]
    pub keep_db: bool,
    #[arg(long, default_value_t = false)]
    pub reuse_db: bool,
    #[arg(long, default_value_t = false)]
    pub skip_prefill: bool,
    #[arg(long, default_value_t = 5_000)]
    pub warmup_reads: u64,
    #[arg(long)]
    pub db_name: Option<String>,
    #[arg(long)]
    pub progress_file: Option<PathBuf>,
    #[arg(long)]
    pub verify_prefix_records: Option<u64>,
    #[arg(long, default_value_t = 0)]
    pub verify_tail_records: u64,
}

#[derive(Debug, Serialize)]
pub struct SingleEvidence {
    #[serde(flatten)]
    setup: SetupTimings,
    #[serde(flatten)]
    work: WorkMeasurement,
    db_bytes: u64,
    peak_rss_kib: u64,
    path: String,
    db_kept: bool,
    reused_db: bool,
    prefill_skipped: bool,
    warmup_reads: u64,
    verification: Option<RecoveryVerification>,
}

impl SingleOptions {
    /// Prepare the caller-selected or generated database name.
    ///
    /// # Errors
    /// Returns invalid path or filesystem errors.
    pub fn prepare(&self, generated_name: &str) -> Result<PathBuf> {
        prepare_database(
            &self.root,
            self.db_name.as_deref().unwrap_or(generated_name),
            self.reuse_db,
        )
    }

    /// Record a committed recovery checkpoint, outside the transaction timing.
    ///
    /// # Errors
    /// Returns an error if the progress marker cannot be written.
    pub fn checkpoint(&self, done: u64) -> Result<()> {
        if let Some(path) = &self.progress_file {
            fs::write(path, format!("{done}\n")).context("write recovery progress marker")?;
        }
        Ok(())
    }

    /// Capture final database size after engine close, retaining verification evidence.
    #[must_use]
    pub fn evidence(
        &self,
        path: &Path,
        setup: SetupTimings,
        work: WorkMeasurement,
        verification: Option<RecoveryVerification>,
    ) -> SingleEvidence {
        SingleEvidence {
            setup,
            work,
            db_bytes: dir_size(path),
            peak_rss_kib: peak_rss_kib(),
            path: path.display().to_string(),
            db_kept: self.keep_db,
            reused_db: self.reuse_db,
            prefill_skipped: self.skip_prefill,
            warmup_reads: self.warmup_reads,
            verification,
        }
    }

    /// Publish the single-client NDJSON record and complete the lifecycle.
    ///
    /// # Errors
    /// Returns serialization or filesystem errors.
    pub fn finish(&self, path: &Path, result: &impl Serialize) -> Result<()> {
        finish_database(
            path,
            self.keep_db,
            result,
            &self.output,
            ReportMode::AppendLine,
        )
    }
}
