#![recursion_limit = "256"]
mod baseline_sizing;
mod compression;
mod cpu_pressure;
mod idle;
mod import_concurrency;
mod io_baseline;
mod io_evidence;
mod io_pressure;
mod noise;
mod planning;
mod process_tree;
mod record_smoke;
mod recovery;
mod scaling;
mod storage;
mod summary;
mod support;
mod sustained;
mod tsdb;
mod tsdb_merge;

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
    AuditBaselineSizing {
        results: PathBuf,
        #[arg(long, default_value_t = 3)]
        expect_trials: i64,
        #[arg(long, default_value_t = 2.0)]
        read_only_min_seconds: f64,
        #[arg(long, default_value_t = 3.0)]
        read_only_target_seconds: f64,
        #[arg(long, default_value_t = 0.75)]
        stateful_min_total_seconds: f64,
        #[arg(long, default_value_t = 1.0)]
        stateful_target_total_seconds: f64,
        #[arg(long, default_value_t = 51)]
        stateful_max_trials: i64,
        #[arg(long, default_value_t = 0.10)]
        max_cv: f64,
        #[arg(long, default_value_t = 0.25)]
        max_relative_spread: f64,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long)]
        markdown_out: Option<PathBuf>,
    },
    RefineStockQualityRepairs {
        quality_plan: PathBuf,
        sizing_audit: PathBuf,
        resize_root: PathBuf,
        #[arg(long)]
        resize_prefix: String,
        #[arg(long)]
        json_out: PathBuf,
        #[arg(long)]
        markdown_out: Option<PathBuf>,
    },
    CpuPressureWorker {
        duty_percent: f64,
    },
    SmokeRecordBinaries {
        #[arg(long)]
        bench_bin: PathBuf,
        #[arg(long)]
        concurrency_bin: PathBuf,
        #[arg(long)]
        sustained_bin: PathBuf,
        #[arg(long)]
        rocks_bench_bin: PathBuf,
        #[arg(long)]
        rocks_concurrency_bin: PathBuf,
        #[arg(long)]
        rocks_sustained_bin: PathBuf,
    },
    CheckExternalNoise {
        #[arg(long, default_value_t = 15)]
        ignore_nice_at_least: i32,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long = "exclude-pid")]
        exclude_pid: Vec<u32>,
        #[arg(long, default_value_t = 50.0)]
        max_aggregate_cpu_percent: f64,
        #[arg(long, default_value_t = 250.0)]
        sample_ms: f64,
    },
    RunWithContinuousNoise {
        #[arg(long)]
        json_out: PathBuf,
        #[arg(long, default_value_t = 250.0)]
        sample_ms: f64,
        #[arg(long, default_value_t = 15)]
        ignore_cpu_nice_at_least: i32,
        #[arg(long, default_value_t = 50.0)]
        max_foreign_cpu_percent: f64,
        #[arg(long, default_value_t = 2.0)]
        max_foreign_io_average_mib_s: f64,
        #[arg(long, default_value_t = 8.0)]
        max_foreign_io_rate_mib_s: f64,
        #[arg(long)]
        observe_only: bool,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
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
    CaptureProcessTree {
        #[arg(long)]
        pid: u32,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    MergeTsdbResult {
        #[arg(long)]
        client: PathBuf,
        #[arg(long = "server-before")]
        server_before: PathBuf,
        #[arg(long = "server-after")]
        server_after: PathBuf,
        #[arg(long = "data-dir")]
        data_dir: PathBuf,
        #[arg(long = "startup-s")]
        startup_s: f64,
        #[arg(long = "server-log")]
        server_log: Option<PathBuf>,
        #[arg(long)]
        output: PathBuf,
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
    SummarizeSustained {
        input: PathBuf,
        #[arg(long)]
        expect_trials: Option<usize>,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long)]
        markdown_out: Option<PathBuf>,
    },
    SummarizeRecovery {
        run_dir: PathBuf,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long)]
        markdown_out: Option<PathBuf>,
    },
    SummarizeIoBaseline {
        run_dir: PathBuf,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long)]
        markdown_out: Option<PathBuf>,
    },
    SummarizeIoPressure {
        run_dir: PathBuf,
        #[arg(long)]
        expect_trials: Option<usize>,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long)]
        markdown_out: Option<PathBuf>,
    },
    ImportConcurrencyResults {
        source: Option<PathBuf>,
        dest: Option<PathBuf>,
        #[arg(long = "exclude-engine")]
        exclude_engine: Vec<String>,
        #[arg(long)]
        verify_run: Option<PathBuf>,
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
        Command::AuditBaselineSizing {
            results,
            expect_trials,
            read_only_min_seconds,
            read_only_target_seconds,
            stateful_min_total_seconds,
            stateful_target_total_seconds,
            stateful_max_trials,
            max_cv,
            max_relative_spread,
            json_out,
            markdown_out,
        } => {
            let code = baseline_sizing::run_audit(baseline_sizing::AuditArgs {
                results: &results,
                expect_trials,
                config: baseline_sizing::SizingConfig {
                    read_only_min_seconds,
                    read_only_target_seconds,
                    stateful_min_total_seconds,
                    stateful_target_total_seconds,
                    stateful_max_trials,
                    max_cv,
                    max_relative_spread,
                },
                json_out: json_out.as_deref(),
                markdown_out: markdown_out.as_deref(),
            })?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::RefineStockQualityRepairs {
            quality_plan,
            sizing_audit,
            resize_root,
            resize_prefix,
            json_out,
            markdown_out,
        } => baseline_sizing::run_refine(baseline_sizing::RefineArgs {
            quality_plan: &quality_plan,
            sizing_audit: &sizing_audit,
            resize_root: &resize_root,
            resize_prefix: &resize_prefix,
            json_out: &json_out,
            markdown_out: markdown_out.as_deref(),
        }),
        Command::CpuPressureWorker { duty_percent } => cpu_pressure::run(duty_percent),
        Command::SmokeRecordBinaries {
            bench_bin,
            concurrency_bin,
            sustained_bin,
            rocks_bench_bin,
            rocks_concurrency_bin,
            rocks_sustained_bin,
        } => record_smoke::run(record_smoke::RecordSmokeArgs {
            bench_bin: &bench_bin,
            concurrency_bin: &concurrency_bin,
            sustained_bin: &sustained_bin,
            rocks_bench_bin: &rocks_bench_bin,
            rocks_concurrency_bin: &rocks_concurrency_bin,
            rocks_sustained_bin: &rocks_sustained_bin,
        }),
        Command::CheckExternalNoise {
            ignore_nice_at_least,
            json_out,
            exclude_pid,
            max_aggregate_cpu_percent,
            sample_ms,
        } => {
            let code = noise::run_external(noise::ExternalNoiseArgs {
                ignore_nice_at_least,
                json_out: json_out.as_deref(),
                exclude_pids: &exclude_pid,
                max_aggregate_cpu_percent,
                sample_ms,
            })?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::RunWithContinuousNoise {
            json_out,
            sample_ms,
            ignore_cpu_nice_at_least,
            max_foreign_cpu_percent,
            max_foreign_io_average_mib_s,
            max_foreign_io_rate_mib_s,
            observe_only,
            command,
        } => {
            let code = noise::run_continuous(noise::ContinuousNoiseArgs {
                json_out: &json_out,
                sample_ms,
                ignore_cpu_nice_at_least,
                max_foreign_cpu_percent,
                max_foreign_io_average_mib_s,
                max_foreign_io_rate_mib_s,
                observe_only,
                command: &command,
            })?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
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
        Command::CaptureProcessTree { pid, output } => {
            let code = process_tree::run(process_tree::ProcessTreeArgs {
                pid,
                output: output.as_deref(),
            })?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::MergeTsdbResult {
            client,
            server_before,
            server_after,
            data_dir,
            startup_s,
            server_log,
            output,
        } => tsdb_merge::run(tsdb_merge::TsdbMergeArgs {
            client: &client,
            server_before: &server_before,
            server_after: &server_after,
            data_dir: &data_dir,
            startup_s,
            server_log: server_log.as_deref(),
            output: &output,
        }),
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
        Command::SummarizeSustained {
            input,
            expect_trials,
            json_out,
            markdown_out,
        } => {
            let code = sustained::run(sustained::SustainedArgs {
                input: &input,
                expect_trials,
                json_out: json_out.as_deref(),
                markdown_out: markdown_out.as_deref(),
            })?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::SummarizeRecovery {
            run_dir,
            json_out,
            markdown_out,
        } => recovery::run(recovery::RecoveryArgs {
            run_dir: &run_dir,
            json_out: json_out.as_deref(),
            markdown_out: markdown_out.as_deref(),
        }),
        Command::SummarizeIoBaseline {
            run_dir,
            json_out,
            markdown_out,
        } => {
            let code = io_baseline::run(io_baseline::IoBaselineArgs {
                run_dir: &run_dir,
                json_out: json_out.as_deref(),
                markdown_out: markdown_out.as_deref(),
            })?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::SummarizeIoPressure {
            run_dir,
            expect_trials,
            json_out,
            markdown_out,
        } => {
            let code = io_pressure::run(io_pressure::IoPressureArgs {
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
        Command::ImportConcurrencyResults {
            source,
            dest,
            exclude_engine,
            verify_run,
        } => import_concurrency::run(import_concurrency::ImportArgs {
            source: source.as_deref(),
            dest: dest.as_deref(),
            excluded_engines: &exclude_engine,
            verify_run: verify_run.as_deref(),
        }),
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
