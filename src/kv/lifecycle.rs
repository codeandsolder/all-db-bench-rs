use anyhow::Result;
use rand::{SeedableRng, rngs::SmallRng};

use super::{
    data::{key, sample_existing_id, value},
    engine::Engine,
    types::{AccessPattern, Durability, KeyShape, ValuePattern, Workload},
};

const PREFILL_BATCH: usize = 10_000;

#[derive(Clone, Copy)]
pub(super) struct DatasetSpec {
    pub(super) records: u64,
    pub(super) key_bytes: usize,
    pub(super) key_shape: KeyShape,
    pub(super) seed: u64,
    pub(super) value_bytes: usize,
    pub(super) value_pattern: ValuePattern,
    pub(super) durability: Durability,
}

pub(super) async fn prefill(
    engine: &mut Engine,
    spec: DatasetSpec,
    extra_records: u64,
) -> Result<()> {
    let mut puts = Vec::with_capacity(PREFILL_BATCH);
    for id in 0..spec.records {
        push_record(spec, id, &mut puts);
        if puts.len() == PREFILL_BATCH {
            flush_batch(engine, spec.durability, &mut puts).await?;
        }
    }
    for slot in 0..extra_records {
        let id = spec.records.saturating_add(slot);
        push_record(spec, id, &mut puts);
        if puts.len() == PREFILL_BATCH {
            flush_batch(engine, spec.durability, &mut puts).await?;
        }
    }
    if !puts.is_empty() {
        engine.write_batch(spec.durability, &puts, &[]).await?;
    }
    Ok(())
}

fn push_record(spec: DatasetSpec, id: u64, puts: &mut Vec<(Vec<u8>, Vec<u8>)>) {
    puts.push((
        key(id, spec.key_bytes, spec.key_shape, spec.seed),
        value(id, spec.value_bytes, spec.seed, spec.value_pattern),
    ));
}

async fn flush_batch(
    engine: &mut Engine,
    durability: Durability,
    puts: &mut Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<()> {
    engine.write_batch(durability, puts, &[]).await?;
    puts.clear();
    Ok(())
}

pub(super) async fn warm_existing_reads(
    engine: &mut Engine,
    spec: DatasetSpec,
    warmup_reads: u64,
    access_pattern: AccessPattern,
    workload: Workload,
) -> Result<()> {
    let mut rng = SmallRng::seed_from_u64(spec.seed ^ 0x7777);
    for _ in 0..spec.records.min(warmup_reads) {
        let id = sample_existing_id(&mut rng, spec.records, access_pattern, workload);
        let _ = engine
            .get(&key(id, spec.key_bytes, spec.key_shape, spec.seed))
            .await?;
    }
    Ok(())
}
