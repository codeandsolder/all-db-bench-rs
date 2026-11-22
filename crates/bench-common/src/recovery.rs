use serde::Serialize;

/// Presence accounting for one ascending range of record IDs.
#[derive(Debug, Default)]
pub struct PresenceCounts {
    missing: u64,
    contiguous_present: u64,
    present_after_gap: u64,
}

impl PresenceCounts {
    /// Record the next ID in ascending order.
    pub const fn observe(&mut self, present: bool) {
        if !present {
            self.missing += 1;
        } else if self.missing == 0 {
            self.contiguous_present += 1;
        } else {
            self.present_after_gap += 1;
        }
    }
}

/// Shared recovery report for KV and record engines.
#[derive(Debug, Serialize)]
pub struct RecoveryVerification {
    expected_prefix_records: u64,
    checked_prefix_records: u64,
    missing_prefix_records: u64,
    prefix_contiguous_present: u64,
    prefix_present_after_gap: u64,
    tail_checked_records: u64,
    tail_prefix_present: u64,
    tail_total_present: u64,
    tail_present_after_gap: u64,
    transaction_atomic_tail: bool,
    #[serde(rename = "verification_ok")]
    ok: bool,
    verify_s: f64,
}

impl RecoveryVerification {
    /// Evaluate the acknowledged prefix and the possibly committed transaction tail.
    #[must_use]
    pub fn from_observations(
        expected_prefix_records: u64,
        tail_records: u64,
        prefix: &PresenceCounts,
        tail: &PresenceCounts,
        txn_size: usize,
        verify_s: f64,
    ) -> Self {
        let txn = txn_size.max(1) as u64;
        let transaction_atomic_tail = tail.contiguous_present.is_multiple_of(txn);
        let ok = prefix.missing == 0
            && prefix.present_after_gap == 0
            && tail.present_after_gap == 0
            && transaction_atomic_tail;
        Self {
            expected_prefix_records,
            checked_prefix_records: expected_prefix_records,
            missing_prefix_records: prefix.missing,
            prefix_contiguous_present: prefix.contiguous_present,
            prefix_present_after_gap: prefix.present_after_gap,
            tail_checked_records: tail_records,
            tail_prefix_present: tail.contiguous_present,
            tail_total_present: tail.contiguous_present + tail.present_after_gap,
            tail_present_after_gap: tail.present_after_gap,
            transaction_atomic_tail,
            ok,
            verify_s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PresenceCounts, RecoveryVerification};

    fn counts(values: &[bool]) -> PresenceCounts {
        let mut counts = PresenceCounts::default();
        for &present in values {
            counts.observe(present);
        }
        counts
    }

    #[test]
    fn recovery_verification_distinguishes_prefix_gaps_and_partial_tails() {
        let prefix = counts(&[true, true]);
        for (tail, ok, atomic) in [
            (counts(&[true, true, false, false]), true, true),
            (counts(&[false, false, false, false]), true, true),
            (counts(&[true, false, false, false]), false, false),
            (counts(&[true, true, false, true]), false, true),
        ] {
            let result = RecoveryVerification::from_observations(2, 4, &prefix, &tail, 2, 0.0);
            assert_eq!(result.ok, ok);
            assert_eq!(result.transaction_atomic_tail, atomic);
            assert_eq!(
                result.tail_total_present,
                tail.contiguous_present + tail.present_after_gap
            );
            assert_eq!(result.tail_present_after_gap, tail.present_after_gap);
        }
        let prefix = counts(&[true, false, true]);
        let result = RecoveryVerification::from_observations(
            3,
            0,
            &prefix,
            &PresenceCounts::default(),
            2,
            0.0,
        );
        assert!(!result.ok);
        assert_eq!(result.missing_prefix_records, 1);
        assert_eq!(result.prefix_contiguous_present, 1);
        assert_eq!(result.prefix_present_after_gap, 1);
    }
}
