use crate::{
    baseline_generation, baseline_selection, baseline_sizing, baseline_supervisor, cli::Command,
    compression, cpu_pressure, idle, import_concurrency, io_baseline, io_pressure, noise, planning,
    process_tree, ready, record_smoke, recovery, scaling, short_pressure, storage, summary,
    support, sustained, tsdb, tsdb_merge,
};
use anyhow::{Context, Result};

fn finish(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        std::process::exit(code)
    }
}

pub(crate) fn run(command: Command) -> Result<()> {
    match command {
        Command::BaselineGeneration {
            repo,
            generation_root,
            generation_id,
            kv_binary_manifest,
            record_binary_manifest,
            lock_file,
            watch,
            busy_sleep,
            dry_run,
        } => {
            let code = baseline_generation::run(baseline_generation::GenerationArgs {
                repo: &repo,
                generation_root: &generation_root,
                generation_id: &generation_id,
                kv_binary_manifest: &kv_binary_manifest,
                record_binary_manifest: &record_binary_manifest,
                lock_file: &lock_file,
                watch,
                busy_sleep,
                dry_run,
            })?;
            finish(code)
        }
        Command::BaselineQuickIdle {
            lane,
            repo,
            bench_bin,
            rocks_bench_bin,
            expected_bench_sha256,
            expected_rocks_bench_sha256,
            run_id,
            lock_file,
            status_file,
            audit_json,
            audit_markdown,
            watch,
            busy_sleep,
            max_io_full_avg10,
        } => {
            let code = baseline_supervisor::run_quick(baseline_supervisor::QuickArgs {
                lane: &lane,
                repo: &repo,
                bench_bin: &bench_bin,
                rocks_bench_bin: rocks_bench_bin.as_deref(),
                expected_bench_sha256: expected_bench_sha256.as_deref(),
                expected_rocks_bench_sha256: expected_rocks_bench_sha256.as_deref(),
                run_id: &run_id,
                lock_file: &lock_file,
                status_file: &status_file,
                audit_json: &audit_json,
                audit_markdown: &audit_markdown,
                watch,
                busy_sleep,
                max_io_full_avg10,
            })?;
            finish(code)
        }
        Command::BaselineSizingFollowups {
            lane,
            repo,
            plan,
            quality_plan,
            run_prefix,
            quality_only,
            bench_bin,
            rocks_bench_bin,
            expected_bench_sha256,
            expected_rocks_bench_sha256,
            lock_file,
            status_file,
            limit,
            watch,
            busy_sleep,
            max_io_full_avg10,
            calibration_count,
            calibration_min_seconds,
            calibration_max_seconds,
            dry_run,
        } => {
            let calibration_max_seconds =
                calibration_max_seconds.unwrap_or(if lane == "record" { 15.0 } else { 6.0 });
            let code = baseline_supervisor::run_followups(baseline_supervisor::FollowupArgs {
                lane: &lane,
                repo: &repo,
                plan: &plan,
                quality_plan: quality_plan.as_deref(),
                run_prefix: &run_prefix,
                quality_only,
                bench_bin: &bench_bin,
                rocks_bench_bin: rocks_bench_bin.as_deref(),
                expected_bench_sha256: expected_bench_sha256.as_deref(),
                expected_rocks_bench_sha256: expected_rocks_bench_sha256.as_deref(),
                lock_file: &lock_file,
                status_file: &status_file,
                limit,
                watch,
                busy_sleep,
                max_io_full_avg10,
                calibration_count,
                calibration_min_seconds,
                calibration_max_seconds,
                dry_run,
            })?;
            finish(code)
        }
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
            finish(code)
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
        Command::ScrubShortTrialPressure {
            run_dir,
            max_elapsed_s,
            max_runqueue_fraction,
            max_cpu_psi_fraction,
            max_benchmark_cpu_share,
            case_id,
            interval,
            dry_run,
        } => short_pressure::run_scrub(
            &run_dir,
            short_pressure::ScrubConfig {
                max_elapsed_s,
                max_runqueue_fraction,
                max_cpu_psi_fraction,
                max_benchmark_cpu_share,
                dry_run,
                cpu_count: None,
                case_ids: case_id.into_iter().collect(),
                intervals: if interval.is_empty() {
                    vec!["measured".to_owned()]
                } else {
                    interval
                },
            },
        ),
        Command::AuditStockShortPressure {
            run_dir,
            sizing_audit,
            lane,
            json_out,
            markdown_out,
        } => short_pressure::run_audit_stock(short_pressure::AuditStockArgs {
            run_dir: &run_dir,
            sizing_audit: &sizing_audit,
            lane: &lane,
            json_out: &json_out,
            markdown_out: markdown_out.as_deref(),
        }),
        Command::SelectBaselineSizingResults {
            stock_results,
            audit,
            resize_root,
            ndjson_out,
            manifest_out,
            resize_prefix,
            quality_repair_plan,
            allow_missing_resize,
        } => baseline_selection::run_select(baseline_selection::SelectArgs {
            stock_results: &stock_results,
            audit: &audit,
            resize_root: &resize_root,
            ndjson_out: &ndjson_out,
            manifest_out: &manifest_out,
            resize_prefix: &resize_prefix,
            quality_repair_plan: quality_repair_plan.as_deref(),
            allow_missing_resize,
        }),
        Command::FinalizeBaselineSizing {
            repo,
            kv_only,
            record_only,
            kv_stock_results,
            kv_audit,
            kv_resize_prefix,
            kv_out_dir,
            kv_quality_repair_plan,
            kv_skip_quality_repairs,
            record_stock_results,
            record_audit,
            record_resize_prefix,
            record_out_dir,
            record_quality_repair_plan,
            record_skip_quality_repairs,
        } => {
            let run_kv = !record_only;
            let run_record = !kv_only;
            let mut lanes = Vec::new();
            if run_kv {
                baseline_selection::finalize_one(
                    &repo,
                    baseline_selection::FinalizeLane {
                        stock_results: kv_stock_results
                            .as_ref()
                            .context("--kv-stock-results is required for the selected lane")?,
                        audit: kv_audit
                            .as_ref()
                            .context("--kv-audit is required for the selected lane")?,
                        resize_prefix: &kv_resize_prefix,
                        out_dir: kv_out_dir
                            .as_ref()
                            .context("--kv-out-dir is required for the selected lane")?,
                        quality_repair_plan: if kv_skip_quality_repairs {
                            None
                        } else {
                            kv_quality_repair_plan.as_deref()
                        },
                        expected_groups: 180,
                    },
                )?;
                lanes.push("kv=180 groups");
            }
            if run_record {
                baseline_selection::finalize_one(
                    &repo,
                    baseline_selection::FinalizeLane {
                        stock_results: record_stock_results
                            .as_ref()
                            .context("--record-stock-results is required for the selected lane")?,
                        audit: record_audit
                            .as_ref()
                            .context("--record-audit is required for the selected lane")?,
                        resize_prefix: &record_resize_prefix,
                        out_dir: record_out_dir
                            .as_ref()
                            .context("--record-out-dir is required for the selected lane")?,
                        quality_repair_plan: if record_skip_quality_repairs {
                            None
                        } else {
                            record_quality_repair_plan.as_deref()
                        },
                        expected_groups: 40,
                    },
                )?;
                lanes.push("record=40 groups");
            }
            println!("final baseline corpora validated: {}", lanes.join(" "));
            Ok(())
        }
        Command::VerifyKvReady {
            selected,
            binary_manifest,
            expected_commit,
            repo,
            stock_run_id,
            expected_admission_policy,
            out,
        } => ready::run_kv(ready::ReadyArgs {
            selected: &selected,
            binary_manifest: &binary_manifest,
            expected_commit: &expected_commit,
            repo: &repo,
            stock_run_id: &stock_run_id,
            expected_admission_policy: &expected_admission_policy,
            out: &out,
        }),
        Command::VerifyRecordReady {
            selected,
            binary_manifest,
            expected_commit,
            repo,
            stock_run_id,
            expected_admission_policy,
            out,
        } => ready::run_record(ready::ReadyArgs {
            selected: &selected,
            binary_manifest: &binary_manifest,
            expected_commit: &expected_commit,
            repo: &repo,
            stock_run_id: &stock_run_id,
            expected_admission_policy: &expected_admission_policy,
            out: &out,
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
            finish(code)
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
            finish(code)
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
            finish(code)
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
            finish(code)
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
            finish(code)
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
            finish(code)
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
            finish(code)
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
            finish(code)
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
            finish(code)
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
