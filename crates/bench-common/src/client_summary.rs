use crate::measurement::count_as_f64;
use anyhow::{Context, Result, bail};
use serde::Serialize;

/// Logical work and client-local timing, captured before reporting.
#[derive(Debug, Serialize)]
pub struct ClientTiming {
    pub client: usize,
    pub ops_requested: u64,
    pub ops_completed: u64,
    pub elapsed_s: f64,
}

#[derive(Debug, Serialize)]
struct ClientMeasurement<T> {
    #[serde(flatten)]
    timing: ClientTiming,
    ops_per_s: f64,
    #[serde(flatten)]
    workload: T,
}

struct Distribution {
    min: f64,
    median: f64,
    max: f64,
}

impl Distribution {
    fn from_samples(mut samples: Vec<f64>) -> Result<Self> {
        samples.sort_unstable_by(f64::total_cmp);
        let min = *samples.first().context("empty client distribution")?;
        let max = *samples.last().context("empty client distribution")?;
        let middle = samples.len() / 2;
        let median = if samples.len().is_multiple_of(2) {
            f64::midpoint(samples[middle - 1], samples[middle])
        } else {
            samples[middle]
        };
        Ok(Self { min, median, max })
    }
}

/// Per-client reports and the distribution of client-local timings/rates.
/// Workload-specific counters and quantiles remain owned by their harness.
#[derive(Debug, Serialize)]
pub struct ClientSummary<T> {
    #[serde(rename = "client_elapsed_s_min")]
    elapsed_min: f64,
    #[serde(rename = "client_elapsed_s_median")]
    elapsed_median: f64,
    #[serde(rename = "client_elapsed_s_max")]
    elapsed_max: f64,
    #[serde(rename = "client_ops_per_s_min")]
    rate_min: f64,
    #[serde(rename = "client_ops_per_s_median")]
    rate_median: f64,
    #[serde(rename = "client_ops_per_s_max")]
    rate_max: f64,
    #[serde(rename = "client_throughput_max_min_ratio")]
    throughput_ratio: f64,
    #[serde(rename = "client_measurements")]
    measurements: Vec<ClientMeasurement<T>>,
}

impl<T> ClientSummary<T> {
    /// Construct aligned per-client and summary metrics from a completed run.
    ///
    /// # Errors
    /// Rejects incomplete, out-of-order or non-finite measurements.
    pub fn from_clients<I>(clients: I) -> Result<Self>
    where
        I: IntoIterator<Item = (ClientTiming, T)>,
    {
        let clients = clients.into_iter();
        let mut reports = Vec::with_capacity(clients.size_hint().0);
        let mut elapsed = Vec::with_capacity(reports.capacity());
        let mut rates = Vec::with_capacity(reports.capacity());
        for (index, (timing, workload)) in clients.enumerate() {
            if timing.client != index || timing.ops_completed != timing.ops_requested {
                bail!("inconsistent client measurement: {timing:?}");
            }
            let rate = count_as_f64(timing.ops_completed) / timing.elapsed_s.max(f64::MIN_POSITIVE);
            if !timing.elapsed_s.is_finite() || timing.elapsed_s < 0.0 || !rate.is_finite() {
                bail!("invalid client timing: {timing:?}");
            }
            elapsed.push(timing.elapsed_s);
            rates.push(rate);
            reports.push(ClientMeasurement {
                timing,
                ops_per_s: rate,
                workload,
            });
        }
        let elapsed = Distribution::from_samples(elapsed)?;
        let rates = Distribution::from_samples(rates)?;
        let ratio = rates.max / rates.min.max(f64::MIN_POSITIVE);
        if !ratio.is_finite() {
            bail!("non-finite client throughput spread");
        }
        Ok(Self {
            elapsed_min: elapsed.min,
            elapsed_median: elapsed.median,
            elapsed_max: elapsed.max,
            rate_min: rates.min,
            rate_median: rates.median,
            rate_max: rates.max,
            throughput_ratio: ratio,
            measurements: reports,
        })
    }
}

/// Timing of the database preparation phases shared by both concurrency lanes.
#[derive(Debug, Serialize)]
pub struct SetupTimings {
    #[serde(rename = "open_s")]
    pub open: f64,
    #[serde(rename = "prefill_s")]
    pub prefill: f64,
    #[serde(rename = "warmup_s")]
    pub warmup: f64,
}

#[cfg(test)]
mod tests {
    use super::{ClientSummary, ClientTiming, SetupTimings};
    use anyhow::Result;
    use serde::Serialize;
    use serde_json::json;

    #[derive(Serialize)]
    struct ExampleWorkload {
        reads: u64,
    }

    fn sample(client: usize, ops: u64, elapsed_s: f64) -> (ClientTiming, ExampleWorkload) {
        (
            ClientTiming {
                client,
                ops_requested: ops,
                ops_completed: ops,
                elapsed_s,
            },
            ExampleWorkload { reads: ops },
        )
    }

    #[test]
    fn per_client_and_distribution_wire_fields_are_flat_and_consistent() -> Result<()> {
        let report = ClientSummary::from_clients([sample(0, 6, 2.0), sample(1, 4, 1.0)])?;
        assert_eq!(
            serde_json::to_value(report)?,
            json!({
                "client_elapsed_s_min": 1.0,
                "client_elapsed_s_median": 1.5,
                "client_elapsed_s_max": 2.0,
                "client_ops_per_s_min": 3.0,
                "client_ops_per_s_median": 3.5,
                "client_ops_per_s_max": 4.0,
                "client_throughput_max_min_ratio": 4.0 / 3.0,
                "client_measurements": [
                    {"client": 0, "ops_requested": 6, "ops_completed": 6, "elapsed_s": 2.0, "ops_per_s": 3.0, "reads": 6},
                    {"client": 1, "ops_requested": 4, "ops_completed": 4, "elapsed_s": 1.0, "ops_per_s": 4.0, "reads": 4}
                ]
            })
        );
        let setup = SetupTimings {
            open: 0.5,
            prefill: 1.5,
            warmup: 0.25,
        };
        assert_eq!(
            serde_json::to_value(setup)?,
            json!({"open_s": 0.5, "prefill_s": 1.5, "warmup_s": 0.25})
        );
        Ok(())
    }

    #[test]
    fn invalid_clients_are_rejected_before_serialization() {
        let (mut timing, workload) = sample(0, 6, 2.0);
        timing.ops_completed = 5;
        assert!(ClientSummary::from_clients([(timing, workload)]).is_err());
        assert!(ClientSummary::from_clients([sample(1, 6, 2.0)]).is_err());
        assert!(ClientSummary::from_clients([sample(0, 6, f64::NAN)]).is_err());
        assert!(ClientSummary::from_clients([sample(0, 6, 0.0)]).is_err());
        assert!(
            ClientSummary::from_clients(std::iter::empty::<(ClientTiming, ExampleWorkload)>())
                .is_err()
        );
    }
}
