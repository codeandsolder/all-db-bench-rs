use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{thread, time::Duration};

pub fn sleep_duration(value: f64) -> Result<Duration> {
    if !value.is_finite() {
        bail!("busy sleep must be finite")
    }
    Duration::try_from_secs_f64(value.max(0.1)).context("busy sleep out of range")
}

#[derive(Debug)]
pub enum RunEvent<'a> {
    Waiting,
    Running,
    Failed { code: i32, error: Option<&'a str> },
}

impl RunEvent<'_> {
    pub fn fields(&self, mut base: Map<String, Value>) -> Map<String, Value> {
        if let Self::Failed { code, error } = self {
            base.insert("returncode".into(), json!(code));
            if let Some(error) = error {
                base.insert("error".into(), json!(error));
            }
        }
        base
    }

    pub const fn state(&self) -> &'static str {
        match self {
            Self::Waiting => "waiting-for-idle",
            Self::Running => "running",
            Self::Failed { .. } => "failed",
        }
    }
}

pub struct AdmissionLoop {
    watch: bool,
    sleep: Duration,
    busy: u64,
}

impl AdmissionLoop {
    pub const fn new(watch: bool, sleep: Duration) -> Self {
        Self {
            watch,
            sleep,
            busy: 0,
        }
    }

    pub const fn busy_events(&self) -> u64 {
        self.busy
    }

    pub fn run(
        &mut self,
        mut admit: impl FnMut() -> Result<i32>,
        mut execute: impl FnMut() -> Result<i32>,
        mut report: impl FnMut(RunEvent<'_>, u64) -> Result<()>,
    ) -> Result<i32> {
        loop {
            let result = match admit() {
                Ok(0) => {
                    report(RunEvent::Running, self.busy)?;
                    execute()
                }
                Ok(code) if code != 75 => {
                    report(
                        RunEvent::Failed {
                            code,
                            error: Some("host preflight failed"),
                        },
                        self.busy,
                    )?;
                    return Ok(code);
                }
                other => other,
            };
            let code = match result {
                Ok(code) => code,
                Err(error) => {
                    report(
                        RunEvent::Failed {
                            code: 2,
                            error: Some(&error.to_string()),
                        },
                        self.busy,
                    )?;
                    return Ok(2);
                }
            };
            match code {
                0 => return Ok(0),
                75 => {
                    self.busy = self.busy.saturating_add(1);
                    report(RunEvent::Waiting, self.busy)?;
                    if !self.watch {
                        return Ok(75);
                    }
                    if self.busy == 1 || self.busy.is_multiple_of(30) {
                        println!("waiting for idle: busy_events={}", self.busy);
                    }
                    thread::sleep(self.sleep);
                }
                code => {
                    report(RunEvent::Failed { code, error: None }, self.busy)?;
                    return Ok(code);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_retries_preflight_and_runner_busy_without_releasing_state() -> Result<()> {
        let mut gate = AdmissionLoop {
            watch: true,
            sleep: Duration::ZERO,
            busy: 0,
        };
        let mut probes = [75, 0, 0].into_iter();
        let mut launches = [75, 0].into_iter();
        let mut events = Vec::new();
        let result = gate.run(
            || probes.next().context("unexpected probe"),
            || launches.next().context("unexpected launch"),
            |event, busy| {
                events.push((event.state(), busy));
                Ok(())
            },
        )?;
        assert_eq!(result, 0);
        assert_eq!(
            events,
            [
                ("waiting-for-idle", 1),
                ("running", 1),
                ("waiting-for-idle", 2),
                ("running", 2)
            ]
        );
        assert_eq!(gate.busy, 2);
        Ok(())
    }

    #[test]
    fn admission_nonwatch_and_failure_do_not_launch() -> Result<()> {
        let mut gate = AdmissionLoop {
            watch: false,
            sleep: Duration::ZERO,
            busy: 0,
        };
        let mut launched = false;
        let mut events = Vec::new();
        let code = gate.run(
            || Ok(75),
            || {
                launched = true;
                Ok(0)
            },
            |event, busy| {
                events.push((event.state(), busy));
                Ok(())
            },
        )?;
        assert_eq!(code, 75);
        assert!(!launched);
        let code = gate.run(
            || bail!("pin verification failed"),
            || {
                launched = true;
                Ok(0)
            },
            |event, busy| {
                assert!(matches!(
                    event,
                    RunEvent::Failed {
                        code: 2,
                        error: Some("pin verification failed")
                    }
                ));
                events.push((event.state(), busy));
                Ok(())
            },
        )?;
        assert_eq!(code, 2);
        assert!(!launched);
        assert_eq!(events, [("waiting-for-idle", 1), ("failed", 1)]);
        Ok(())
    }

    #[test]
    fn fatal_preflight_is_reported_and_child_is_not_launched() -> Result<()> {
        let mut gate = AdmissionLoop::new(true, Duration::ZERO);
        let result = gate.run(
            || Ok(9),
            || bail!("must not launch"),
            |event, busy| {
                assert!(matches!(
                    event,
                    RunEvent::Failed {
                        code: 9,
                        error: Some("host preflight failed")
                    }
                ));
                assert_eq!(busy, 0);
                Ok(())
            },
        )?;
        assert_eq!(result, 9);
        assert!(sleep_duration(f64::INFINITY).is_err());
        assert!(sleep_duration(f64::MAX).is_err());
        Ok(())
    }
}
