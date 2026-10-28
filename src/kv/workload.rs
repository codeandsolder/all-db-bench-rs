use super::types::Workload;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MixedEpochTargets {
    pub reads: u64,
    pub updates: u64,
    pub inserts: u64,
    pub deletes: u64,
}

impl MixedEpochTargets {
    #[must_use]
    pub fn total(self) -> u64 {
        self.reads + self.updates + self.inserts + self.deletes
    }
}

#[must_use]
pub(crate) fn mixed_epoch_targets(
    workload: Workload,
    epoch: u64,
    bounded_state: bool,
) -> MixedEpochTargets {
    match workload {
        Workload::ReadHeavy => {
            let updates = epoch * 5 / 100;
            MixedEpochTargets {
                reads: epoch - updates,
                updates,
                inserts: 0,
                deletes: 0,
            }
        }
        Workload::Balanced => {
            let updates = epoch * 30 / 100;
            let inserts = epoch * 10 / 100;
            let deletes = inserts;
            MixedEpochTargets {
                reads: epoch - updates - inserts - deletes,
                updates,
                inserts,
                deletes,
            }
        }
        Workload::Churn if bounded_state => {
            let inserts = epoch * 30 / 100;
            let deletes = inserts;
            MixedEpochTargets {
                reads: 0,
                updates: epoch - inserts - deletes,
                inserts,
                deletes,
            }
        }
        Workload::Churn => {
            let updates = epoch * 40 / 100;
            let inserts = epoch * 30 / 100;
            MixedEpochTargets {
                reads: 0,
                updates,
                inserts,
                deletes: epoch - updates - inserts,
            }
        }
        _ => unreachable!("mixed_epoch_targets called for non-mixed workload"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BoundedChurnSwap {
    pub delete_id: u64,
    pub put_id: u64,
}

#[must_use]
pub(crate) fn bounded_churn_swap(
    records: u64,
    total_slots: u64,
    slot: u64,
    active_a: bool,
) -> BoundedChurnSwap {
    let a_id = records.saturating_add(slot);
    let b_id = records.saturating_add(total_slots).saturating_add(slot);
    if active_a {
        BoundedChurnSwap {
            delete_id: a_id,
            put_id: b_id,
        }
    } else {
        BoundedChurnSwap {
            delete_id: b_id,
            put_id: a_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_churn_pairs_are_disjoint_and_reversible() {
        let records = 100_000;
        let slots = 8_192;
        for slot in [0, 1, slots / 2, slots - 1] {
            let forward = bounded_churn_swap(records, slots, slot, true);
            let reverse = bounded_churn_swap(records, slots, slot, false);
            assert_eq!(forward.delete_id, reverse.put_id);
            assert_eq!(forward.put_id, reverse.delete_id);
            assert_ne!(forward.delete_id, forward.put_id);
            assert!((records..records + slots).contains(&forward.delete_id));
            assert!((records + slots..records + 2 * slots).contains(&forward.put_id));
        }
    }

    #[test]
    fn mixed_epoch_targets_preserve_exact_logical_op_count() {
        for epoch in 1..=1_000 {
            for workload in [Workload::ReadHeavy, Workload::Balanced, Workload::Churn] {
                for bounded in [false, true] {
                    let targets = mixed_epoch_targets(workload, epoch, bounded);
                    assert_eq!(targets.total(), epoch);
                    if bounded && matches!(workload, Workload::Balanced | Workload::Churn) {
                        assert_eq!(targets.inserts, targets.deletes);
                    }
                }
            }
        }
    }
}
