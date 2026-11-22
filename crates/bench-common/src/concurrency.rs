use crate::metrics::{ProcDelta, ProcSnapshot, SystemDelta, SystemSnapshot};
use anyhow::{Context, Result, bail};
use std::{
    sync::{Arc, Condvar, Mutex, PoisonError, mpsc},
    thread::{self, JoinHandle},
    time::Instant,
};

/// A contiguous range of logical operations or dataset slots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkPartition {
    pub count: u64,
    pub offset: u64,
}

/// Divide a range evenly, keeping each partition aligned to whole units.
///
/// # Errors
/// Rejects zero clients, zero granularity or an unaligned total.
pub fn partition_work(total: u64, clients: usize, granularity: u64) -> Result<Vec<WorkPartition>> {
    if clients == 0 || granularity == 0 || !total.is_multiple_of(granularity) {
        bail!(
            "invalid work partition: total={total}, clients={clients}, granularity={granularity}"
        );
    }
    let clients = u64::try_from(clients)?;
    let units = total / granularity;
    let base = units / clients;
    let extra = units % clients;
    let mut offset = 0;
    Ok((0..clients)
        .map(|client| {
            let count = (base + u64::from(client < extra)) * granularity;
            let partition = WorkPartition { count, offset };
            offset += count;
            partition
        })
        .collect())
}

#[derive(PartialEq, Eq)]
enum Phase {
    Waiting(usize),
    Running,
    Finished,
    Cancelled,
}

/// Cancellable start barrier and completion latch. Workers remain alive
/// after sending their outcomes until the coordinator captures schedstat.
struct Gate {
    phase: Mutex<Phase>,
    changed: Condvar,
}

impl Gate {
    const fn new(participants: usize) -> Self {
        Self {
            phase: Mutex::new(Phase::Waiting(participants)),
            changed: Condvar::new(),
        }
    }

    fn arrive(&self) -> bool {
        let mut phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        if let Phase::Waiting(remaining) = &mut *phase {
            *remaining -= 1;
            if *remaining == 0 {
                *phase = Phase::Running;
                drop(phase);
                self.changed.notify_all();
                return true;
            }
        }
        let phase = self
            .changed
            .wait_while(phase, |phase| matches!(phase, Phase::Waiting(_)))
            .unwrap_or_else(PoisonError::into_inner);
        let arrived = matches!(*phase, Phase::Running | Phase::Finished);
        drop(phase);
        arrived
    }

    fn wait_for_finish(&self) {
        let phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        drop(
            self.changed
                .wait_while(phase, |phase| {
                    !matches!(*phase, Phase::Finished | Phase::Cancelled)
                })
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    fn finish(&self) {
        let mut phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        *phase = if matches!(*phase, Phase::Waiting(_) | Phase::Cancelled) {
            Phase::Cancelled
        } else {
            Phase::Finished
        };
        drop(phase);
        self.changed.notify_all();
    }
}

struct Workers {
    gate: Arc<Gate>,
    joins: Vec<JoinHandle<()>>,
}

impl Workers {
    fn spawn<F, T>(
        &mut self,
        name: String,
        client: usize,
        mut task: F,
        tx: mpsc::Sender<(usize, Result<T>)>,
    ) -> Result<()>
    where
        F: FnMut() -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let gate = Arc::clone(&self.gate);
        self.joins
            .push(thread::Builder::new().name(name).spawn(move || {
                if gate.arrive() {
                    let _ = tx.send((client, task()));
                    // A completed worker must not keep the result channel open.
                    // Otherwise a panicking peer can deadlock the receiver.
                    drop(tx);
                    gate.wait_for_finish();
                    // Keep the captured backend handle alive through the snapshot.
                    drop(task);
                }
            })?);
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.gate.finish();
        let mut panicked = false;
        for join in self.joins.drain(..) {
            panicked |= join.join().is_err();
        }
        if panicked {
            bail!("concurrency client thread panicked");
        }
        Ok(())
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        // Includes partial thread creation and unwinding in the coordinator.
        let _ = self.finish();
    }
}

/// Completed workers in input order and the surrounding measurement window.
pub struct WorkerMeasurement<T> {
    pub clients: Vec<T>,
    pub elapsed_s: f64,
    pub process: ProcDelta,
    pub system_before: SystemSnapshot,
    pub system_after: SystemSnapshot,
    pub system_delta: SystemDelta,
}

fn receive<T>(rx: &mpsc::Receiver<(usize, Result<T>)>, count: usize) -> Result<Vec<(usize, T)>> {
    let mut outcomes = Vec::with_capacity(count);
    for _ in 0..count {
        let (client, result) = rx
            .recv()
            .context("client result channel closed before all clients completed")?;
        outcomes.push((
            client,
            result.with_context(|| format!("client {client} failed"))?,
        ));
    }
    Ok(outcomes)
}

/// Run thread-confined client tasks with a synchronized start and measured completion window.
/// Tasks need not be Sync; they own their backend handles.
///
/// # Errors
/// Returns errors from thread creation, worker tasks or panicked workers.
pub fn run_workers<F, T>(thread_prefix: &str, tasks: Vec<F>) -> Result<WorkerMeasurement<T>>
where
    F: FnMut() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let count = tasks.len();
    let participants = count.checked_add(1).context("worker count overflow")?;
    let mut workers = Workers {
        gate: Arc::new(Gate::new(participants)),
        joins: Vec::with_capacity(count),
    };
    let (tx, rx) = mpsc::channel();
    for (client, task) in tasks.into_iter().enumerate() {
        workers.spawn(
            format!("{thread_prefix}-c{client}"),
            client,
            task,
            tx.clone(),
        )?;
    }
    drop(tx);

    let system_before = SystemSnapshot::capture();
    let process_before = ProcSnapshot::capture();
    let started = Instant::now();
    workers.gate.arrive();
    let outcomes = receive(&rx, count);
    let elapsed = started.elapsed();
    let process_after = ProcSnapshot::capture();
    let system_after = SystemSnapshot::capture();

    workers.finish()?;
    let mut outcomes = outcomes?;
    outcomes.sort_unstable_by_key(|(client, _)| *client);
    Ok(WorkerMeasurement {
        clients: outcomes.into_iter().map(|(_, result)| result).collect(),
        elapsed_s: elapsed.as_secs_f64(),
        process: process_before.delta(&process_after, elapsed),
        system_delta: system_before.delta(&system_after),
        system_before,
        system_after,
    })
}

#[cfg(test)]
mod tests {
    use super::{Gate, WorkPartition, Workers, partition_work, run_workers};
    use anyhow::{Result, bail};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };

    #[test]
    fn partitions_preserve_total_and_alignment() -> Result<()> {
        assert_eq!(
            partition_work(18, 4, 3)?,
            vec![
                WorkPartition {
                    count: 6,
                    offset: 0
                },
                WorkPartition {
                    count: 6,
                    offset: 6
                },
                WorkPartition {
                    count: 3,
                    offset: 12
                },
                WorkPartition {
                    count: 3,
                    offset: 15
                },
            ]
        );
        assert_eq!(
            partition_work(0, 3, 1)?,
            vec![
                WorkPartition {
                    count: 0,
                    offset: 0
                };
                3
            ]
        );
        assert!(partition_work(7, 3, 2).is_err());
        assert!(partition_work(7, 0, 1).is_err());
        Ok(())
    }

    #[test]
    fn workers_return_input_order_and_all_finish_on_task_error() -> Result<()> {
        let completed = Arc::new(AtomicUsize::new(0));
        let tasks = (0..4)
            .map(|client| {
                let completed = Arc::clone(&completed);
                move || {
                    completed.fetch_add(1, Ordering::Relaxed);
                    if client == 1 {
                        bail!("injected failure");
                    }
                    Ok(client)
                }
            })
            .collect();
        assert!(run_workers("test-error", tasks).is_err());
        assert_eq!(completed.load(Ordering::Relaxed), 4);
        let tasks = (0..4).map(|client| move || Ok(client)).collect();
        assert_eq!(run_workers("test-order", tasks)?.clients, vec![0, 1, 2, 3]);
        Ok(())
    }

    #[test]
    fn incomplete_launch_releases_waiting_workers() -> Result<()> {
        let (tx, _rx) = mpsc::channel::<(usize, Result<()>)>();
        let mut workers = Workers {
            gate: Arc::new(Gate::new(3)),
            joins: vec![],
        };
        let called = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&called);
        workers.spawn(
            "incomplete-launch".to_owned(),
            0,
            move || {
                captured.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
            tx,
        )?;
        drop(workers);
        assert_eq!(called.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[test]
    fn closed_result_channel_does_not_wait_for_completed_worker_handles() -> Result<()> {
        // Model a peer exiting without sending a result, as with a task panic.
        // The surviving worker still waits for the finish latch.
        let (tx, rx) = mpsc::channel::<(usize, Result<()>)>();
        let mut workers = Workers {
            gate: Arc::new(Gate::new(2)),
            joins: vec![],
        };
        workers.spawn("surviving-worker".to_owned(), 0, || Ok(()), tx)?;
        assert!(workers.gate.arrive());
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        workers.finish()?;
        Ok(())
    }
}
