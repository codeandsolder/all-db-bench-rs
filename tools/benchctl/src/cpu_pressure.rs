use anyhow::{Result, bail};
use std::{
    thread,
    time::{Duration, Instant},
};

const PERIOD_S: f64 = 0.020;

fn duty_durations(duty_percent: f64) -> Result<(Duration, Duration)> {
    if !duty_percent.is_finite() || !(0.0 < duty_percent && duty_percent <= 100.0) {
        bail!("DUTY_PERCENT must be in (0, 100]");
    }
    let busy_s = PERIOD_S * duty_percent / 100.0;
    let sleep_s = (PERIOD_S - busy_s).max(0.0);
    Ok((
        Duration::from_secs_f64(busy_s),
        Duration::from_secs_f64(sleep_s),
    ))
}

pub(crate) fn run(duty_percent: f64) -> Result<()> {
    let (busy, sleep) = duty_durations(duty_percent)?;
    loop {
        let deadline = Instant::now() + busy;
        while Instant::now() < deadline {}
        if !sleep.is_zero() {
            thread::sleep(sleep);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PERIOD_S, duty_durations};
    use anyhow::Result;

    #[test]
    fn duty_cycle_preserves_twenty_millisecond_period() -> Result<()> {
        for duty in [1.0, 25.0, 50.0, 75.0, 100.0] {
            let (busy, sleep) = duty_durations(duty)?;
            assert!((busy.as_secs_f64() + sleep.as_secs_f64() - PERIOD_S).abs() < 1e-12);
            assert!((busy.as_secs_f64() / PERIOD_S * 100.0 - duty).abs() < 1e-9);
        }
        Ok(())
    }

    #[test]
    fn invalid_duty_cycles_are_rejected() {
        for duty in [-1.0, 0.0, 100.1, f64::NAN, f64::INFINITY] {
            assert!(duty_durations(duty).is_err());
        }
    }
}
