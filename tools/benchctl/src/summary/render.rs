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

    let lanes = summary
        .iter()
        .map(|group| group.lane.as_str())
        .collect::<BTreeSet<_>>();
    for lane in lanes {
        lines.extend([format!("# {lane} lane"), String::new()]);
        let durabilities = summary
            .iter()
            .filter(|group| group.lane == lane)
            .map(|group| group.durability.as_str())
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
                .filter(|group| group.lane == lane && group.durability == durability)
                .collect::<Vec<_>>();
            block.sort_by(|left, right| {
                left.scenario
                    .cmp(&right.scenario)
                    .then(left.workload.cmp(&right.workload))
                    .then_with(|| right.ops_per_s_median.total_cmp(&left.ops_per_s_median))
            });
            for group in block {
                let p99 = group.p99_read_us_median.or(group.p99_operation_us_median);
                lines.push(format!(
                    "| {} | {} | {} | {} | {} {} | {} | {} | {}–{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    group.format_version,
                    group.scenario,
                    group.workload,
                    group.configuration,
                    group.engine,
                    group.engine_version,
                    group.trials.len(),
                    fmt(group.ops_per_s_median),
                    fmt(group.ops_per_s_q1),
                    fmt(group.ops_per_s_q3),
                    opt_fmt(group.speedup_vs_c1),
                    opt_fmt(group.parallel_efficiency_vs_c1),
                    opt_fmt(group.p99_read_multiplier_vs_c1),
                    opt_fmt(group.p99_write_txn_multiplier_vs_c1),
                    opt_fmt(group.cpu_cores_median),
                    opt_fmt(group.client_throughput_max_min_ratio_median),
                    opt_fmt(group.client_setup_s_median.map(|value| value * 1000.0)),
                    opt_fmt(group.write_conflict_retries_per_k_write_ops_median),
                    opt_fmt(p99),
                    opt_fmt(group.p99_write_txn_us_median),
                    fmt(group.db_bytes_median / (1024.0 * 1024.0)),
                    opt_fmt(group.peak_rss_kib_median.map(|value| value / 1024.0)),
                    opt_fmt(group.open_s_median.map(|value| value * 1000.0)),
                    opt_fmt(group.prefill_s_median),
                    opt_fmt(group.cpu_ns_per_op_median),
                    opt_fmt(group.runqueue_wait_fraction_median.map(|value| value * 100.0)),
                    opt_fmt(group.cpu_psi_some_fraction_median.map(|value| value * 100.0)),
                    opt_fmt(group.read_bytes_per_op_median),
                    opt_fmt(group.write_bytes_per_op_median),
                    opt_fmt(group.io_psi_full_fraction_median.map(|value| value * 100.0)),
                    opt_fmt(group.swap_activity_pages_median),
                ));
            }
            lines.push(String::new());
        }
    }
    lines.join("\n")
}
