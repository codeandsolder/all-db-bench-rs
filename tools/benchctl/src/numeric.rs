use anyhow::{Context, Result, bail};
use num_traits::ToPrimitive;

/// Round a nonnegative integer up to the next multiple without overflowing.
pub fn align_up(value: i64, quantum: i64) -> Result<i64> {
    if value < 0 || quantum <= 0 {
        bail!("invalid alignment: value={value}, quantum={quantum}");
    }
    let groups = value / quantum + i64::from(value % quantum != 0);
    groups
        .checked_mul(quantum)
        .context("aligned value exceeds integer range")
}

/// Linear interpolation in a sorted sample. Reject invalid quantile requests
/// and empty samples; callers decide whether to filter nonfinite observations.
pub fn sample_quantile(values: &mut [f64], q: f64) -> Option<f64> {
    if values.is_empty() || !(0.0..=1.0).contains(&q) {
        return None;
    }
    values.sort_unstable_by(f64::total_cmp);
    let position = q * int_as_f64!(values.len() - 1);
    let low = position.floor().to_usize()?;
    let high = position.ceil().to_usize()?;
    if low == high {
        values.get(low).copied()
    } else {
        let fraction = position - int_as_f64!(low);
        Some(
            values
                .get(high)?
                .mul_add(fraction, *values.get(low)? * (1.0 - fraction)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{align_up, sample_quantile};
    use anyhow::Result;

    #[test]
    fn align_up_is_checked_at_integer_limits() -> Result<()> {
        assert_eq!(align_up(0, 5)?, 0);
        assert_eq!(align_up(21, 5)?, 25);
        assert_eq!(align_up(i64::MAX, 1)?, i64::MAX);
        assert!(align_up(i64::MAX, 2).is_err());
        assert!(align_up(-1, 5).is_err());
        assert!(align_up(1, 0).is_err());
        Ok(())
    }

    #[test]
    fn quantile_interpolates_and_checks_bounds() {
        let mut sample = [4.0, 1.0, 3.0, 2.0];
        assert_eq!(
            sample_quantile(&mut sample, 0.25).map(f64::to_bits),
            Some(1.75_f64.to_bits())
        );
        assert_eq!(
            sample_quantile(&mut sample, 0.75).map(f64::to_bits),
            Some(3.25_f64.to_bits())
        );
        assert_eq!(
            sample_quantile(&mut sample, 1.0).map(f64::to_bits),
            Some(4.0_f64.to_bits())
        );
        assert!(sample_quantile(&mut sample, -0.01).is_none());
        assert!(sample_quantile(&mut sample, f64::NAN).is_none());
        assert!(sample_quantile(&mut [], 0.5).is_none());
    }
}
