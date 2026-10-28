#![recursion_limit = "256"]
mod baseline_generation;
mod baseline_selection;
mod baseline_sizing;
mod baseline_supervisor;
mod cli;
mod compression;
mod cpu_pressure;
mod dispatch;
mod file_hash;
mod idle;
mod import_concurrency;
mod io_baseline;
mod io_evidence;
mod io_pressure;
mod json_io;
mod json_value;
mod noise;
mod planning;
mod process_tree;
mod ready;
mod record_smoke;
mod recovery;
mod scaling;
mod short_pressure;
mod storage;
mod summary;
mod support;
mod sustained;
mod tsdb;
mod tsdb_merge;

use anyhow::Result;
use clap::Parser;
use cli::Cli;

fn main() -> Result<()> {
    dispatch::run(Cli::parse().command)
}
