mod compression;
mod idle;
mod planning;
mod scaling;
mod storage;
mod summary;
mod support;
mod tsdb;

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
    SummarizeTsdb {
        run_dir: PathBuf,
        #[arg(long)]
        expect_trials: Option<usize>,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long)]
        markdown_out: Option<PathBuf>,
    },
    ConcurrencySizingAudit {
        #[arg(long)]
        repo: PathBuf,
        run_dir: PathBuf,
        #[arg(long = "additional-run")]
        additional_run: Vec<PathBuf>,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long)]
        markdown_out: Option<PathBuf>,
        #[arg(long)]
        probe_plan_out: Option<PathBuf>,
        #[arg(long)]
        expected_admission_policy: Option<String>,
    },
    KvConcurrencyPlanCalibration {
        sizing_audit: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    KvConcurrencyPlanMaterialize {
        plan: PathBuf,
        tsv_out: PathBuf,
        meta_out: PathBuf,
    },
    KvConcurrencyPlanFinal {
        #[arg(long)]
        repo: PathBuf,
        sizing_audit: PathBuf,
        calibration_plan: PathBuf,
        calibration_run: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        delete_out: PathBuf,
        #[arg(long, default_value_t = 3)]
        trials: i64,
    },
    RecordConcurrencyPlanCalibration {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    RecordConcurrencyPlanMaterialize {
        plan: PathBuf,
        tsv_out: Option<PathBuf>,
        meta_out: Option<PathBuf>,
    },
    ConcurrencyScalingSummary {
        run_dir: PathBuf,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long)]
        markdown_out: Option<PathBuf>,
        #[arg(long)]
        allow_incomplete: bool,
    },
    RecordConcurrencyPlanFinal {
        #[arg(long)]
        repo: PathBuf,
        calibration_plan: PathBuf,
        calibration_run: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 3)]
        trials: i64,
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
        Command::SummarizeTsdb {
            run_dir,
            expect_trials,
            json_out,
            markdown_out,
        } => {
            let code = tsdb::run(tsdb::TsdbSummaryArgs {
                run_dir: &run_dir,
                expect_trials,
                json_out: json_out.as_deref(),
                markdown_out: markdown_out.as_deref(),
            })?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::ConcurrencySizingAudit {
            repo,
            run_dir,
            additional_run,
            json_out,
            markdown_out,
            probe_plan_out,
            expected_admission_policy,
        } => planning::run_concurrency_sizing_audit(planning::AuditArgs {
            repo: &repo,
            run_dir: &run_dir,
            additional_runs: &additional_run,
            json_out: json_out.as_deref(),
            markdown_out: markdown_out.as_deref(),
            probe_plan_out: probe_plan_out.as_deref(),
            expected_admission_policy: expected_admission_policy.as_deref(),
        }),
        Command::KvConcurrencyPlanCalibration { sizing_audit, out } => {
            planning::run_kv_calibration(planning::KvCalibrationArgs {
                sizing_audit: &sizing_audit,
                out: &out,
            })
        }
        Command::KvConcurrencyPlanMaterialize {
            plan,
            tsv_out,
            meta_out,
        } => planning::run_kv_materialize(planning::KvMaterializeArgs {
            plan: &plan,
            tsv_out: &tsv_out,
            meta_out: &meta_out,
        }),
        Command::KvConcurrencyPlanFinal {
            repo,
            sizing_audit,
            calibration_plan,
            calibration_run,
            out,
            delete_out,
            trials,
        } => planning::run_kv_final(planning::KvFinalArgs {
            repo: &repo,
            sizing_audit: &sizing_audit,
            calibration_plan: &calibration_plan,
            calibration_run: &calibration_run,
            out: &out,
            delete_out: &delete_out,
            trials,
        }),
        Command::RecordConcurrencyPlanCalibration { repo, out } => {
            planning::run_record_calibration(planning::RecordCalibrationArgs {
                repo: &repo,
                out: &out,
            })
        }
        Command::RecordConcurrencyPlanMaterialize {
            plan,
            tsv_out,
            meta_out,
        } => planning::run_record_materialize(planning::MaterializeArgs {
            plan: &plan,
            tsv_out: tsv_out.as_deref(),
            meta_out: meta_out.as_deref(),
        }),
        Command::ConcurrencyScalingSummary {
            run_dir,
            json_out,
            markdown_out,
            allow_incomplete,
        } => {
            let code = scaling::run(scaling::ScalingArgs {
                run_dir: &run_dir,
                json_out: json_out.as_deref(),
                markdown_out: markdown_out.as_deref(),
                allow_incomplete,
            })?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::RecordConcurrencyPlanFinal {
            repo,
            calibration_plan,
            calibration_run,
            out,
            trials,
        } => planning::run_record_final(planning::RecordFinalArgs {
            repo: &repo,
            calibration_plan: &calibration_plan,
            calibration_run: &calibration_run,
            out: &out,
            trials,
        }),
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
