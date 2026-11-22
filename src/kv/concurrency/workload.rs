use super::Args;
use crate::kv::{
    backend_ops::BackendOps,
    data::{key, mix_u64},
    fixed_work::{ChurnPool, WorkExecutor, WorkMode, WorkStats},
};
use all_db_bench_common::{client_summary::ClientTiming, concurrency::WorkPartition};
use anyhow::{Result, bail};
use hdrhistogram::Histogram;
use std::time::Instant;

#[derive(Debug)]
pub(super) struct ClientRun {
    pub(super) timing: ClientTiming,
    pub(super) reads: u64,
    pub(super) writes: u64,
    pub(super) deletes: u64,
    pub(super) write_conflict_retries: u64,
    pub(super) read_hist: Histogram<u64>,
    pub(super) write_hist: Histogram<u64>,
}

pub(super) async fn run_client<C: BackendOps + Send>(
    engine: &mut C,
    args: &Args,
    client: usize,
    ops: WorkPartition,
    slots: WorkPartition,
) -> Result<ClientRun> {
    let salt = mix_u64((client as u64).wrapping_add(0x636c_6965_6e74_0001));
    let pool = if args.uses_churn_pool() {
        Some(ChurnPool::new(args.bounded_churn_slots, slots)?)
    } else {
        None
    };
    let mut executor = WorkExecutor::new(
        args.work_spec(),
        ops,
        WorkMode::Concurrent,
        args.seed ^ u64::from(args.trial) ^ salt,
        pool,
        WorkStats::new()?,
    )?;
    let started = Instant::now();
    executor.run(engine).await?;
    let elapsed_s = started.elapsed().as_secs_f64();
    let stats = executor.stats;
    Ok(ClientRun {
        timing: ClientTiming {
            client,
            ops_requested: ops.count,
            ops_completed: stats.completed,
            elapsed_s,
        },
        reads: stats.reads,
        writes: stats.writes,
        deletes: stats.deletes,
        write_conflict_retries: stats.write_conflict_retries,
        read_hist: stats.read_hist,
        write_hist: stats.write_hist,
    })
}

pub(super) async fn verify_bounded_churn_state<C: BackendOps + Send>(
    client: &mut C,
    args: &Args,
) -> Result<()> {
    for slot in 0..args.bounded_churn_slots {
        let a_id = args.records.saturating_add(slot);
        let b_id = args
            .records
            .saturating_add(args.bounded_churn_slots)
            .saturating_add(slot);
        let a = client
            .read(&key(a_id, args.key_bytes, args.key_shape, args.seed))
            .await?
            .is_some();
        let b = client
            .read(&key(b_id, args.key_bytes, args.key_shape, args.seed))
            .await?
            .is_some();
        if a == b {
            bail!("bounded churn slot {slot} has invalid occupancy: a={a} b={b}");
        }
    }
    Ok(())
}
