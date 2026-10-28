use super::{Args, StateEvolution};
use crate::kv::{
    concurrency_clients::ClientOps,
    data::{key, mix_u64, sample_existing_id, sample_read_id, value},
    metrics::{hist, record},
    types::{AccessPattern, KeyShape, Workload, WritePattern},
    workload::{bounded_churn_swap, mixed_epoch_targets},
};
use anyhow::{Result, bail};
use hdrhistogram::Histogram;
use rand::{RngExt, SeedableRng, rngs::SmallRng, seq::SliceRandom};
use std::time::Instant;

#[derive(Debug)]
pub(super) struct ClientRun {
    pub(super) client: usize,
    pub(super) ops_requested: u64,
    pub(super) ops_completed: u64,
    pub(super) elapsed_s: f64,
    pub(super) reads: u64,
    pub(super) writes: u64,
    pub(super) deletes: u64,
    pub(super) write_conflict_retries: u64,
    pub(super) read_hist: Histogram<u64>,
    pub(super) write_hist: Histogram<u64>,
}

pub(super) async fn run_client<C: ClientOps>(
    engine: &mut C,
    args: &Args,
    client: usize,
    ops: u64,
    offset: u64,
    churn_slot_start: u64,
    churn_slot_count: u64,
) -> Result<ClientRun> {
    let salt = mix_u64((client as u64).wrapping_add(0x636c_6965_6e74_0001));
    let mut rng = SmallRng::seed_from_u64(args.seed ^ u64::from(args.trial) ^ salt);
    let mut read_hist = hist();
    let mut write_hist = hist();
    let mut reads = 0u64;
    let mut writes = 0u64;
    let mut deletes = 0u64;
    let mut write_conflict_retries = 0u64;
    let mut done = 0u64;
    let txn_size = args.txn_size.max(1);
    let mut next_id = args.records.saturating_add(offset);
    let mut churn_active_a = vec![true; churn_slot_count as usize];
    let mut churn_cursor = 0u64;
    let make_key = |id| key(id, args.key_bytes, args.key_shape, args.seed);
    let make_value = |id, salt| value(id, args.value_bytes, salt, args.value_pattern);

    let started = Instant::now();
    match args.workload {
        Workload::PointRead => {
            for _ in 0..ops {
                let (id, expect_hit) = sample_read_id(
                    &mut rng,
                    args.records,
                    args.ops,
                    args.miss_percent,
                    args.access_pattern,
                    args.workload,
                );
                let t = Instant::now();
                let got = engine.get(&make_key(id)).await?;
                record(&mut read_hist, t.elapsed());
                if got.is_some() != expect_hit {
                    bail!("client {client} point-read mismatch id={id} expected_hit={expect_hit}");
                }
                reads += 1;
                done += 1;
            }
        }
        Workload::RangeScan => {
            if !args.engine.supports_range_scan() {
                bail!("selected engine has no ordered keyed range-scan API");
            }
            if args.key_shape == KeyShape::Hashed {
                bail!("hashed keys have no ID-ordered range semantics");
            }
            if args.scan_len == 0 || args.scan_len > args.records {
                bail!("scan_len must be in 1..=records");
            }
            let last_start = args.records - args.scan_len;
            for _ in 0..ops {
                let start_id = if last_start == 0 {
                    0
                } else {
                    rng.random_range(0..=last_start)
                };
                let end_id = start_id + args.scan_len;
                let t = Instant::now();
                let n = engine
                    .scan_count(&make_key(start_id), &make_key(end_id))
                    .await?;
                record(&mut read_hist, t.elapsed());
                if n != args.scan_len as usize {
                    bail!(
                        "client {client} range scan returned {n}, expected {}",
                        args.scan_len
                    );
                }
                reads += args.scan_len;
                done += 1;
            }
        }
        Workload::TinyTxn | Workload::WriteBurst => {
            let batch = if matches!(args.workload, Workload::TinyTxn) {
                1
            } else {
                txn_size
            };
            while done < ops {
                let n = (ops - done).min(batch as u64) as usize;
                let mut puts = Vec::with_capacity(n);
                for local in 0..n {
                    let id = match args.write_pattern {
                        WritePattern::Append => {
                            let id = next_id;
                            next_id += 1;
                            id
                        }
                        WritePattern::UpdateUniform => rng.random_range(0..args.records),
                        WritePattern::UpdateHot => sample_existing_id(
                            &mut rng,
                            args.records,
                            match args.access_pattern {
                                AccessPattern::Auto | AccessPattern::Uniform => {
                                    AccessPattern::Hot80
                                }
                                other => other,
                            },
                            args.workload,
                        ),
                    };
                    puts.push((
                        make_key(id),
                        make_value(id, args.seed ^ 0x1111 ^ offset ^ done ^ local as u64),
                    ));
                }
                let t = Instant::now();
                write_conflict_retries = write_conflict_retries
                    .saturating_add(engine.write_batch(args.durability, &puts, &[]).await?);
                record(&mut write_hist, t.elapsed());
                writes += n as u64;
                done += n as u64;
            }
        }
        Workload::DeleteBurst => {
            if args.ops > args.records {
                bail!("delete-burst requires total ops <= records");
            }
            while done < ops {
                let n = (ops - done).min(txn_size as u64) as usize;
                let mut dels = Vec::with_capacity(n);
                for local in 0..n {
                    dels.push(make_key(offset + done + local as u64));
                }
                let t = Instant::now();
                write_conflict_retries = write_conflict_retries
                    .saturating_add(engine.write_batch(args.durability, &[], &dels).await?);
                record(&mut write_hist, t.elapsed());
                deletes += n as u64;
                done += n as u64;
            }
        }
        Workload::ReadHeavy | Workload::Balanced | Workload::Churn => {
            enum WriteSpec {
                Put(Vec<u8>, Vec<u8>),
                Delete(Vec<u8>),
                Toggle {
                    put: (Vec<u8>, Vec<u8>),
                    delete: Vec<u8>,
                },
            }
            enum Unit {
                Read(Vec<u8>),
                Write {
                    puts: Vec<(Vec<u8>, Vec<u8>)>,
                    deletes: Vec<Vec<u8>>,
                },
            }

            while done < ops {
                let epoch = (ops - done).min(1_000);
                let targets = mixed_epoch_targets(
                    args.workload,
                    epoch,
                    args.state_evolution == StateEvolution::Bounded,
                );
                debug_assert_eq!(targets.total(), epoch);

                let mut units = Vec::with_capacity((targets.reads as usize) + 16);
                for _ in 0..targets.reads {
                    let (id, _expect_hit) = sample_read_id(
                        &mut rng,
                        args.records,
                        args.ops,
                        args.miss_percent,
                        args.access_pattern,
                        args.workload,
                    );
                    units.push(Unit::Read(make_key(id)));
                }

                let mut specs = Vec::with_capacity(
                    (targets.updates + targets.inserts + targets.deletes) as usize,
                );
                for _ in 0..targets.updates {
                    let id = sample_existing_id(
                        &mut rng,
                        args.records,
                        args.access_pattern,
                        args.workload,
                    );
                    specs.push(WriteSpec::Put(
                        make_key(id),
                        make_value(id, args.seed ^ offset ^ done),
                    ));
                }
                if args.state_evolution == StateEvolution::Bounded
                    && matches!(args.workload, Workload::Balanced | Workload::Churn)
                {
                    if targets.inserts != targets.deletes {
                        bail!("bounded churn requires paired insert/delete targets");
                    }
                    for _ in 0..targets.inserts {
                        let local_slot = (churn_cursor % churn_slot_count) as usize;
                        churn_cursor = churn_cursor.saturating_add(1);
                        let slot = churn_slot_start.saturating_add(local_slot as u64);
                        let swap = bounded_churn_swap(
                            args.records,
                            args.bounded_churn_slots,
                            slot,
                            churn_active_a[local_slot],
                        );
                        let delete_id = swap.delete_id;
                        let put_id = swap.put_id;
                        churn_active_a[local_slot] = !churn_active_a[local_slot];
                        specs.push(WriteSpec::Toggle {
                            put: (
                                make_key(put_id),
                                make_value(put_id, args.seed ^ offset ^ done ^ churn_cursor),
                            ),
                            delete: make_key(delete_id),
                        });
                    }
                } else {
                    for _ in 0..targets.inserts {
                        let id = next_id;
                        next_id += 1;
                        specs.push(WriteSpec::Put(
                            make_key(id),
                            make_value(id, args.seed ^ offset ^ done),
                        ));
                    }
                    for _ in 0..targets.deletes {
                        let id = sample_existing_id(
                            &mut rng,
                            args.records,
                            args.access_pattern,
                            args.workload,
                        );
                        specs.push(WriteSpec::Delete(make_key(id)));
                    }
                }
                specs.shuffle(&mut rng);

                let mut puts = Vec::with_capacity(txn_size);
                let mut dels = Vec::with_capacity(txn_size);
                let mut batch_ops = 0usize;
                for spec in specs {
                    let spec_ops = match &spec {
                        WriteSpec::Toggle { .. } => 2,
                        WriteSpec::Put(_, _) | WriteSpec::Delete(_) => 1,
                    };
                    if batch_ops != 0 && batch_ops + spec_ops > txn_size {
                        units.push(Unit::Write {
                            puts: std::mem::take(&mut puts),
                            deletes: std::mem::take(&mut dels),
                        });
                        batch_ops = 0;
                    }
                    match spec {
                        WriteSpec::Put(k, v) => puts.push((k, v)),
                        WriteSpec::Delete(k) => dels.push(k),
                        WriteSpec::Toggle { put, delete } => {
                            puts.push(put);
                            dels.push(delete);
                        }
                    }
                    batch_ops += spec_ops;
                    if batch_ops == txn_size {
                        units.push(Unit::Write {
                            puts: std::mem::take(&mut puts),
                            deletes: std::mem::take(&mut dels),
                        });
                        batch_ops = 0;
                    }
                }
                if batch_ops != 0 {
                    units.push(Unit::Write {
                        puts,
                        deletes: dels,
                    });
                }
                units.shuffle(&mut rng);

                for unit in units {
                    match unit {
                        Unit::Read(k) => {
                            let t = Instant::now();
                            let _ = engine.get(&k).await?;
                            record(&mut read_hist, t.elapsed());
                            reads += 1;
                            done += 1;
                        }
                        Unit::Write {
                            puts,
                            deletes: dels,
                        } => {
                            let n = puts.len() + dels.len();
                            let t = Instant::now();
                            write_conflict_retries = write_conflict_retries.saturating_add(
                                engine.write_batch(args.durability, &puts, &dels).await?,
                            );
                            record(&mut write_hist, t.elapsed());
                            writes += puts.len() as u64;
                            deletes += dels.len() as u64;
                            done += n as u64;
                        }
                    }
                }
            }
        }
    }

    Ok(ClientRun {
        client,
        ops_requested: ops,
        ops_completed: done,
        elapsed_s: started.elapsed().as_secs_f64(),
        reads,
        writes,
        deletes,
        write_conflict_retries,
        read_hist,
        write_hist,
    })
}

pub(super) async fn verify_bounded_churn_state<C: ClientOps>(
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
            .get(&key(a_id, args.key_bytes, args.key_shape, args.seed))
            .await?
            .is_some();
        let b = client
            .get(&key(b_id, args.key_bytes, args.key_shape, args.seed))
            .await?
            .is_some();
        if a == b {
            bail!("bounded churn slot {slot} has invalid occupancy: a={a} b={b}");
        }
    }
    Ok(())
}
