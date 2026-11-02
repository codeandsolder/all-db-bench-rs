#![recursion_limit = "256"]

macro_rules! int_as_f64 {
    ($value:expr) => {{
        #[expect(
            clippy::cast_precision_loss,
            reason = "benchmark analysis intentionally projects bounded integer counters into f64"
        )]
        let value = $value as f64;
        value
    }};
}

macro_rules! f64_to_i64 {
    ($value:expr) => {{
        #[expect(
            clippy::cast_possible_truncation,
            reason = "benchmark planning intentionally rounds finite bounded f64 control values to i64"
        )]
        let value = $value as i64;
        value
    }};
}

macro_rules! f64_to_i32 {
    ($value:expr) => {{
        #[expect(
            clippy::cast_possible_truncation,
            reason = "base-10 planning exponents are finite and tiny before conversion to i32"
        )]
        let value = $value as i32;
        value
    }};
}

macro_rules! f64_to_u64 {
    ($value:expr) => {{
        #[expect(
            clippy::cast_possible_truncation,
            reason = "validated nonnegative timing and byte values intentionally narrow to u64"
        )]
        #[expect(
            clippy::cast_sign_loss,
            reason = "validated nonnegative timing and byte values intentionally narrow to u64"
        )]
        let value = $value as u64;
        value
    }};
}

macro_rules! numeric_to_usize {
    ($value:expr) => {{
        #[expect(
            clippy::cast_possible_truncation,
            reason = "validated nonnegative bounded indices and counts intentionally narrow to usize"
        )]
        #[expect(
            clippy::cast_sign_loss,
            reason = "validated nonnegative bounded indices and counts intentionally narrow to usize"
        )]
        let value = $value as usize;
        value
    }};
}
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
