use crate::{RecordData, backend::RecordBackend};
use all_db_bench_common::record_payload::{PayloadPattern, payload_for};
use anyhow::{Result, bail};
use rand::{RngExt, SeedableRng, rngs::SmallRng};

#[derive(Clone, Copy)]
pub struct RecordDataset {
    pub records: u64,
    pub payload_bytes: usize,
    pub payload_pattern: PayloadPattern,
    pub seed: u64,
}

pub enum Warmup {
    Sequential,
    Random { seed: u64 },
}

impl RecordDataset {
    pub fn row(self, id: u64, salt: u64) -> RecordData {
        RecordData {
            bucket: (id % 100) as u32,
            payload: payload_for(self.payload_pattern, id, self.payload_bytes, salt),
        }
    }

    pub async fn prefill<B: RecordBackend>(self, engine: &mut B, batch: usize) -> Result<()> {
        if self.records == 0 || batch == 0 {
            bail!("prefill requires positive record count and batch size");
        }
        let mut rows = Vec::with_capacity(batch);
        for id in 0..self.records {
            rows.push((id, self.row(id, self.seed)));
            if rows.len() == batch {
                engine.upsert_batch(&rows).await?;
                rows.clear();
            }
        }
        if !rows.is_empty() {
            engine.upsert_batch(&rows).await?;
        }
        Ok(())
    }

    pub async fn warm<B: RecordBackend>(
        self,
        engine: &mut B,
        requested: u64,
        order: Warmup,
    ) -> Result<()> {
        let count = self.records.min(requested);
        match order {
            Warmup::Sequential => {
                for id in 0..count {
                    std::hint::black_box(engine.get(id).await?);
                }
            }
            Warmup::Random { seed } => {
                let mut rng = SmallRng::seed_from_u64(seed);
                for _ in 0..count {
                    let id = rng.random_range(0..self.records);
                    let _ = engine.get(id).await?;
                }
            }
        }
        Ok(())
    }
}
