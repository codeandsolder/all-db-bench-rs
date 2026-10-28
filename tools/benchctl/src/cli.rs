use crate::{ready, support};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "all-db-benchctl")]
#[command(about = "Native support tooling for all-db-bench-rs")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    BaselineGeneration {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        generation_root: PathBuf,
        #[arg(long)]
        generation_id: String,
        #[arg(long)]
        kv_binary_manifest: PathBuf,
        #[arg(long)]
        record_binary_manifest: PathBuf,
        #[arg(long, default_value = "/run/lock/all-db-bench-performance.lock")]
        lock_file: PathBuf,
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 2.0)]
        busy_sleep: f64,
        #[arg(long)]
        dry_run: bool,
    },
    BaselineQuickIdle {
        #[arg(long)]
        lane: String,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        bench_bin: PathBuf,
        #[arg(long)]
        rocks_bench_bin: Option<PathBuf>,
        #[arg(long)]
        expected_bench_sha256: Option<String>,
        #[arg(long)]
        expected_rocks_bench_sha256: Option<String>,
        #[arg(long)]
        run_id: String,
        #[arg(long, default_value = "/run/lock/all-db-bench-performance.lock")]
        lock_file: PathBuf,
        #[arg(long)]
        status_file: PathBuf,
        #[arg(long)]
        audit_json: PathBuf,
        #[arg(long)]
        audit_markdown: PathBuf,
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 2.0)]
        busy_sleep: f64,
        #[arg(long, default_value_t = 5.0)]
        max_io_full_avg10: f64,
    },
    BaselineSizingFollowups {
        #[arg(long)]
        lane: String,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        quality_plan: Option<PathBuf>,
        #[arg(long)]
        run_prefix: String,
        #[arg(long)]
        quality_only: bool,
        #[arg(long)]
        bench_bin: PathBuf,
        #[arg(long)]
        rocks_bench_bin: Option<PathBuf>,
        #[arg(long)]
        expected_bench_sha256: Option<String>,
        #[arg(long)]
        expected_rocks_bench_sha256: Option<String>,
        #[arg(long, default_value = "/run/lock/all-db-bench-performance.lock")]
        lock_file: PathBuf,
        #[arg(long)]
        status_file: PathBuf,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 2.0)]
        busy_sleep: f64,
        #[arg(long, default_value_t = 5.0)]
        max_io_full_avg10: f64,
        #[arg(long, default_value_t = 5)]
        calibration_count: usize,
        #[arg(long, default_value_t = 1.5)]
        calibration_min_seconds: f64,
        #[arg(long)]
        calibration_max_seconds: Option<f64>,
        #[arg(long)]
        dry_run: bool,
    },
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
    ScrubShortTrialPressure {
        run_dir: PathBuf,
        #[arg(long, default_value_t = 0.5)]
        max_elapsed_s: f64,
        #[arg(long, default_value_t = 0.03)]
        max_runqueue_fraction: f64,
        #[arg(long, default_value_t = 0.05)]
        max_cpu_psi_fraction: f64,
        #[arg(long, default_value_t = 0.5)]
        max_benchmark_cpu_share: f64,
        #[arg(long = "case-id")]
        case_id: Vec<String>,
        #[arg(long = "interval", value_parser = ["measured", "open"])]
        interval: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    AuditStockShortPressure {
        run_dir: PathBuf,
        sizing_audit: PathBuf,
        #[arg(long, value_parser = ["kv", "record"])]
        lane: String,
        #[arg(long)]
        json_out: PathBuf,
        #[arg(long)]
        markdown_out: Option<PathBuf>,
    },
    SelectBaselineSizingResults {
        stock_results: PathBuf,
        audit: PathBuf,
        resize_root: PathBuf,
        #[arg(long)]
        ndjson_out: PathBuf,
        #[arg(long)]
        manifest_out: PathBuf,
        #[arg(long, default_value = "20261006-kv-resize-v4")]
        resize_prefix: String,
        #[arg(long)]
        quality_repair_plan: Option<PathBuf>,
        #[arg(long)]
        allow_missing_resize: bool,
    },
    FinalizeBaselineSizing {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long, conflicts_with = "record_only")]
        kv_only: bool,
        #[arg(long, conflicts_with = "kv_only")]
        record_only: bool,
        #[arg(long)]
        kv_stock_results: Option<PathBuf>,
        #[arg(long)]
        kv_audit: Option<PathBuf>,
        #[arg(long, default_value = "kv-resize")]
        kv_resize_prefix: String,
        #[arg(long)]
        kv_out_dir: Option<PathBuf>,
        #[arg(long)]
        kv_quality_repair_plan: Option<PathBuf>,
        #[arg(long)]
        kv_skip_quality_repairs: bool,
        #[arg(long)]
        record_stock_results: Option<PathBuf>,
        #[arg(long)]
        record_audit: Option<PathBuf>,
        #[arg(long, default_value = "record-resize")]
        record_resize_prefix: String,
        #[arg(long)]
        record_out_dir: Option<PathBuf>,
        #[arg(long)]
        record_quality_repair_plan: Option<PathBuf>,
        #[arg(long)]
        record_skip_quality_repairs: bool,
    },
    VerifyKvReady {
        #[arg(long)]
        selected: PathBuf,
        #[arg(long)]
        binary_manifest: PathBuf,
        #[arg(long)]
        expected_commit: String,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        stock_run_id: String,
        #[arg(long, default_value = ready::CONTINUOUS_ADMISSION_POLICY)]
        expected_admission_policy: String,
        #[arg(long)]
        out: PathBuf,
    },
    VerifyRecordReady {
        #[arg(long)]
        selected: PathBuf,
        #[arg(long)]
        binary_manifest: PathBuf,
        #[arg(long)]
        expected_commit: String,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        stock_run_id: String,
        #[arg(long)]
        expected_admission_policy: String,
        #[arg(long)]
        out: PathBuf,
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
