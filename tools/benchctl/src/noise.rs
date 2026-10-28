mod continuous;
mod external;
mod proc;

pub(crate) use continuous::run_continuous;
pub(crate) use external::run_external;

use std::path::Path;

#[derive(Debug)]
pub(crate) struct ExternalNoiseArgs<'a> {
    pub(crate) ignore_nice_at_least: i32,
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) exclude_pids: &'a [u32],
    pub(crate) max_aggregate_cpu_percent: f64,
    pub(crate) sample_ms: f64,
}

#[derive(Debug)]
pub(crate) struct ContinuousNoiseArgs<'a> {
    pub(crate) json_out: &'a Path,
    pub(crate) sample_ms: f64,
    pub(crate) ignore_cpu_nice_at_least: i32,
    pub(crate) max_foreign_cpu_percent: f64,
    pub(crate) max_foreign_io_average_mib_s: f64,
    pub(crate) max_foreign_io_rate_mib_s: f64,
    pub(crate) observe_only: bool,
    pub(crate) command: &'a [String],
}

#[cfg(test)]
mod tests {
    use super::{
        continuous::{ProcStat, contamination_reasons, continuous_descendants},
        external::{
            ProcessRow, aggregate_foreign_cpu_percent, classify_process, cpu_percent_from_samples,
            descendant_pids, is_kernel_thread, low_priority_sccache_tree,
        },
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn row(nice: i32, cpu: f64, comm: &str, args: &str) -> ProcessRow {
        ProcessRow {
            pid: 123,
            ppid: 1,
            nice,
            cpu_percent: cpu,
            comm: comm.to_owned(),
            args: args.to_owned(),
        }
    }

    #[test]
    fn external_classification_matches_policy() {
        assert_eq!(
            classify_process(&row(19, 95.0, "rustc", "rustc crate.rs"), 15),
            None
        );
        assert_eq!(
            classify_process(
                &row(0, 5.0, "clippy-driver", "clippy-driver rustc src/lib.rs"),
                15
            ),
            Some("compiler-or-build")
        );
        assert_eq!(
            classify_process(
                &row(
                    0,
                    5.0,
                    "cargo-clippy",
                    "/root/.rustup/toolchains/nightly/bin/cargo-clippy clippy --fix"
                ),
                15
            ),
            Some("compiler-or-build")
        );
        assert_eq!(
            classify_process(
                &row(0, 2.0, "find", "find /root /srv /opt /home /mnt -type f"),
                15
            ),
            Some("wide-filesystem-scan")
        );
        assert_eq!(
            classify_process(&row(0, 75.0, "python3", "python3 worker.py"), 15),
            Some("foreign-high-cpu")
        );
        assert_eq!(
            classify_process(&row(0, 3.0, "tailscaled", "/usr/sbin/tailscaled"), 15),
            None
        );
    }

    #[test]
    fn kernel_threads_are_ignored() {
        let kernel = ProcessRow {
            pid: 900,
            ppid: 2,
            nice: -19,
            cpu_percent: 120.0,
            comm: "z_wr_iss".to_owned(),
            args: "[z_wr_iss]".to_owned(),
        };
        assert!(is_kernel_thread(&kernel));
        assert_eq!(classify_process(&kernel, 15), None);
        let user = ProcessRow {
            pid: 901,
            ppid: 1,
            nice: 0,
            cpu_percent: 20.0,
            comm: "python".to_owned(),
            args: "python worker.py".to_owned(),
        };
        assert_eq!(
            aggregate_foreign_cpu_percent(&[kernel, user], &BTreeSet::new(), 15),
            20.0
        );
    }

    #[test]
    fn descendant_and_sccache_trees_close_transitively() {
        let rows = vec![
            ProcessRow {
                pid: 200,
                ppid: 100,
                nice: 0,
                cpu_percent: 100.0,
                comm: "ps".into(),
                args: "ps".into(),
            },
            ProcessRow {
                pid: 201,
                ppid: 200,
                nice: 0,
                cpu_percent: 100.0,
                comm: "helper".into(),
                args: "helper child".into(),
            },
            ProcessRow {
                pid: 400,
                ppid: 1,
                nice: 10,
                cpu_percent: 20.0,
                comm: "sccache-dist".into(),
                args: "/usr/local/bin/sccache-dist server --config x".into(),
            },
            ProcessRow {
                pid: 401,
                ppid: 400,
                nice: 10,
                cpu_percent: 100.0,
                comm: "cc1plus".into(),
                args: "/usr/lib/gcc/cc1plus file.cc".into(),
            },
        ];
        assert_eq!(
            descendant_pids(&rows, &BTreeSet::from([100])),
            BTreeSet::from([100, 200, 201])
        );
        assert_eq!(
            low_priority_sccache_tree(&rows, 10),
            BTreeSet::from([400, 401])
        );
    }

    #[test]
    fn aggregate_cpu_catches_many_medium_workers() {
        let rows = vec![
            ProcessRow {
                pid: 800,
                ppid: 1,
                nice: 0,
                cpu_percent: 20.0,
                comm: "python".into(),
                args: "python a.py".into(),
            },
            ProcessRow {
                pid: 801,
                ppid: 1,
                nice: 0,
                cpu_percent: 20.0,
                comm: "python".into(),
                args: "python b.py".into(),
            },
            ProcessRow {
                pid: 802,
                ppid: 1,
                nice: 0,
                cpu_percent: 20.0,
                comm: "python".into(),
                args: "python c.py".into(),
            },
            ProcessRow {
                pid: 803,
                ppid: 1,
                nice: 19,
                cpu_percent: 95.0,
                comm: "python".into(),
                args: "python low.py".into(),
            },
            ProcessRow {
                pid: 804,
                ppid: 1,
                nice: 0,
                cpu_percent: 100.0,
                comm: "kvbench".into(),
                args: "/tmp/kvbench --engine redb".into(),
            },
        ];
        assert_eq!(
            aggregate_foreign_cpu_percent(&rows, &BTreeSet::new(), 15),
            160.0
        );
    }

    #[test]
    fn cpu_percent_uses_interval_and_handles_new_process() {
        assert!(
            (cpu_percent_from_samples(Some((10_000, 123)), (10_025, 123), 0.25, 1000.0, 100)
                - 100.0)
                .abs()
                < 1e-9
        );
        assert!(
            (cpu_percent_from_samples(None, (25, 99_975), 0.25, 1000.0, 100) - 100.0).abs() < 1e-9
        );
    }

    #[test]
    fn continuous_descendants_close_over_tree() {
        let meta = BTreeMap::from([
            (
                10,
                ProcStat {
                    ppid: 1,
                    nice: 0,
                    cpu_ticks: 0,
                    start_ticks: 100,
                },
            ),
            (
                11,
                ProcStat {
                    ppid: 10,
                    nice: 0,
                    cpu_ticks: 0,
                    start_ticks: 101,
                },
            ),
            (
                12,
                ProcStat {
                    ppid: 11,
                    nice: 0,
                    cpu_ticks: 0,
                    start_ticks: 102,
                },
            ),
            (
                20,
                ProcStat {
                    ppid: 1,
                    nice: 0,
                    cpu_ticks: 0,
                    start_ticks: 200,
                },
            ),
        ]);
        assert_eq!(
            continuous_descendants(&meta, &BTreeSet::from([10])),
            BTreeSet::from([10, 11, 12])
        );
    }

    #[test]
    fn contamination_thresholds_match_python_contract() {
        assert!(contamination_reasons(1024.0, 1024.0, 10.0, 1.0, 1.0, 50.0).is_empty());
        assert_eq!(
            contamination_reasons(
                2.0 * 1024.0 * 1024.0,
                2.0 * 1024.0 * 1024.0,
                50.0,
                1.0,
                1.0,
                50.0,
            ),
            ["foreign-io-average", "foreign-io-rate", "foreign-cpu"]
        );
    }
}
