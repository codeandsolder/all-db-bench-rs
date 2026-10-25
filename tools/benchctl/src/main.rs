mod compression;
mod idle;
mod storage;
mod summary;
mod support;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "all-db-benchctl")]
#[command(about = "Native support tooling for all-db-bench-rs")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    StorageFootprint {
        path: PathBuf,
    },
    Summarize {
        input: PathBuf,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long)]
        markdown_out: Option<PathBuf>,
        #[arg(long)]
        expect_trials: Option<usize>,
    },
    SummarizeCompression {
        input: PathBuf,
        #[arg(long)]
        json_out: PathBuf,
        #[arg(long)]
        markdown_out: PathBuf,
    },
    ConcurrencyPlanIdle {
        #[arg(long)]
        lane: String,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        bench_bin: PathBuf,
        #[arg(long)]
        bench_bin_sha256: String,
        #[arg(long)]
        rocks_bench_bin: Option<PathBuf>,
        #[arg(long)]
        rocks_bench_bin_sha256: Option<String>,
        #[arg(long)]
        lock_file: Option<PathBuf>,
        #[arg(long)]
        status_file: PathBuf,
        #[arg(long, default_value_t = 1.0)]
        busy_sleep: f64,
    },
    SupportIdentity {
        support: String,
        #[arg(long)]
        expected_lane: String,
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        runner_name: Option<String>,
        #[arg(long, default_value = support::CONTINUOUS_ADMISSION)]
        expected_admission: String,
        #[arg(long)]
        plan: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::StorageFootprint { path } => storage::print_footprint(&path),
        Command::Summarize {
            input,
            json_out,
            markdown_out,
            expect_trials,
        } => summary::summarize_file(
            &input,
            json_out.as_deref(),
            markdown_out.as_deref(),
            expect_trials,
        ),
        Command::SummarizeCompression {
            input,
            json_out,
            markdown_out,
        } => compression::summarize_file(&input, &json_out, &markdown_out),
        Command::ConcurrencyPlanIdle {
            lane,
            repo,
            plan,
            run_id,
            bench_bin,
            bench_bin_sha256,
            rocks_bench_bin,
            rocks_bench_bin_sha256,
            lock_file,
            status_file,
            busy_sleep,
        } => {
            let code = idle::run(idle::IdleArgs {
                lane: &lane,
                repo: &repo,
                plan: &plan,
                run_id: &run_id,
                bench_bin: &bench_bin,
                bench_bin_sha256: &bench_bin_sha256,
                rocks_bench_bin: rocks_bench_bin.as_deref(),
                rocks_bench_bin_sha256: rocks_bench_bin_sha256.as_deref(),
                lock_file: lock_file.as_deref(),
                status_file: &status_file,
                busy_sleep_s: busy_sleep,
            })?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::SupportIdentity {
            support,
            expected_lane,
            repo,
            runner_name,
            expected_admission,
            plan,
        } => support::run_support_identity(
            &support,
            &expected_lane,
            repo.as_deref(),
            runner_name.as_deref(),
            &expected_admission,
            plan.as_deref(),
        ),
    }
}
