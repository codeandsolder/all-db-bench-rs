use super::model::SummaryGroup;
use std::collections::BTreeSet;

fn integer_with_commas(value: f64) -> String {
    let rounded = format!("{value:.0}");
    let (sign, digits) = rounded
        .strip_prefix('-')
        .map_or(("", rounded.as_str()), |digits| ("-", digits));
    let mut reversed = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().rev().enumerate() {
        if index > 0 && index.is_multiple_of(3) {
            reversed.push(',');
        }
        reversed.push(character);
    }
    format!("{sign}{}", reversed.chars().rev().collect::<String>())
}

fn fmt(value: f64) -> String {
    if value.is_nan() {
        "—".to_owned()
    } else if value.abs() >= 1000.0 {
        integer_with_commas(value)
    } else if value.abs() >= 100.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn opt_fmt(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), fmt)
}

pub(super) fn markdown(summary: &[SummaryGroup], problems: &[String]) -> String {
    let mut lines = Vec::new();
    if !problems.is_empty() {
        lines.extend(["# Completeness warnings".to_owned(), String::new()]);
        lines.extend(problems.iter().map(|problem| format!("- {problem}")));
        lines.push(String::new());
    }

    let lane_names = summary
        .iter()
        .map(|group| group.key.lane.as_str())
        .collect::<BTreeSet<_>>();
    for lane in lane_names {
        lines.extend([format!("# {lane} lane"), String::new()]);
        let durabilities = summary
            .iter()
            .filter(|group| group.key.lane == lane)
            .map(|group| group.key.durability.as_str())
            .collect::<BTreeSet<_>>();
        for durability in durabilities {
            lines.extend([
                format!("## {durability}"),
                String::new(),
                "| schema | scenario | workload | config | engine | trials | median ops/s | IQR ops/s | speedup vs c1 | efficiency | p99 read/op ×c1 | p99 write-txn ×c1 | CPU cores | client max/min | client setup ms | conflict retries/k write ops | median p99 read/op us | median p99 write-txn us | DB MiB | peak RSS MiB | open ms | prefill s | CPU ns/op | rq wait %wall | CPU PSI some %wall | read B/op | write B/op | IO PSI full %wall | swap pages |".to_owned(),
                "|---:|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
            ]);
            let mut block = summary
                .iter()
                .filter(|group| group.key.lane == lane && group.key.durability == durability)
                .collect::<Vec<_>>();
            block.sort_by(|left, right| {
                left.key
                    .scenario
                    .cmp(&right.key.scenario)
                    .then(left.key.workload.cmp(&right.key.workload))
                    .then_with(|| right.throughput.median.total_cmp(&left.throughput.median))
            });
            for group in block {
                let p99 = group.p99_read_us_median.or(group.p99_operation_us_median);
                lines.push(format!(
                    "| {} | {} | {} | {} | {} {} | {} | {} | {}–{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    group.key.format_version,
                    group.key.scenario,
                    group.key.workload,
                    group.configuration,
                    group.key.engine,
                    group.key.engine_version,
                    group.trials.len(),
                    fmt(group.throughput.median),
                    fmt(group.throughput.q1),
                    fmt(group.throughput.q3),
                    opt_fmt(group.speedup_vs_c1),
                    opt_fmt(group.parallel_efficiency_vs_c1),
                    opt_fmt(group.p99_read_multiplier_vs_c1),
                    opt_fmt(group.p99_write_txn_multiplier_vs_c1),
                    opt_fmt(group.process.cpu_cores),
                    opt_fmt(group.client_throughput_max_min_ratio_median),
                    opt_fmt(group.client_setup_s_median.map(|value| value * 1000.0)),
                    opt_fmt(group.write_conflict_retries_per_k_write_ops_median),
                    opt_fmt(p99),
                    opt_fmt(group.p99_write_txn_us_median),
                    fmt(group.db_bytes_median / (1024.0 * 1024.0)),
                    opt_fmt(group.peak_rss_kib_median.map(|value| value / 1024.0)),
                    opt_fmt(group.open_s_median.map(|value| value * 1000.0)),
                    opt_fmt(group.prefill_s_median),
                    opt_fmt(group.process.cpu_ns_per_op),
                    opt_fmt(group.process.runqueue_wait_fraction.map(|value| value * 100.0)),
                    opt_fmt(group.system.cpu_psi_some_fraction.map(|value| value * 100.0)),
                    opt_fmt(group.process.read_bytes_per_op),
                    opt_fmt(group.process.write_bytes_per_op),
                    opt_fmt(group.system.io_psi_full_fraction.map(|value| value * 100.0)),
                    opt_fmt(group.system.swap_activity_pages),
                ));
            }
            lines.push(String::new());
        }
    }
    lines.join("\n")
}
