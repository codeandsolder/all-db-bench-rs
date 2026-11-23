/*
 * Copyright (c) Microsoft Corporation.
 * Licensed under the MIT license.
 */

//! Task spawning for the index's parallel phases.
//!
//! Where a tokio runtime exists, [`spawn`] and [`JoinSet`] are tokio's: the caller must be
//! inside a runtime context, as for [`tokio::spawn`].
//!
//! On `wasm32-unknown-unknown` (browsers and edge workers) there is no tokio runtime and no
//! threads. There a spawned future runs on the JavaScript event loop through
//! `wasm_bindgen_futures::spawn_local`, concurrently with the spawner and with every other
//! spawned future, so futures that wait on one another (a provider that batches requests from
//! several tasks, say) make progress as they do under tokio. As with tokio, dropping a handle
//! detaches its task rather than cancelling it.

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use tokio::task::{JoinError, JoinHandle, JoinSet};

/// Spawns `future` as a task and returns its handle.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(future)
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub use local::{JoinError, JoinHandle, JoinSet, spawn};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod local {
    use std::{
        collections::VecDeque,
        fmt,
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    use tokio::sync::oneshot;

    /// The error of a task that ended without producing its output: it panicked, or the
    /// executor dropped it before it completed.
    #[derive(Debug)]
    pub struct JoinError;

    impl fmt::Display for JoinError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("task ended without producing its output")
        }
    }

    impl std::error::Error for JoinError {}

    /// The output of a spawned task, available once the task completes.
    pub struct JoinHandle<T> {
        output: oneshot::Receiver<T>,
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Pin::new(&mut self.output)
                .poll(cx)
                .map(|output| output.map_err(|_| JoinError))
        }
    }

    /// Spawns `future` on the JavaScript event loop and returns its handle.
    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (done, output) = oneshot::channel();
        wasm_bindgen_futures::spawn_local(async move {
            // The send fails only when the handle was dropped, which detaches the task: its
            // output has no reader.
            let _ = done.send(future.await);
        });
        JoinHandle { output }
    }

    /// A set of spawned tasks, which all run concurrently from the moment they are spawned.
    /// [`Self::join_next`] returns their outputs in spawn order.
    pub struct JoinSet<T> {
        pending: VecDeque<JoinHandle<T>>,
    }

    impl<T: Send + 'static> Default for JoinSet<T> {
        fn default() -> Self {
            Self::new()
        }
    }

    impl<T: Send + 'static> JoinSet<T> {
        pub fn new() -> Self {
            Self {
                pending: VecDeque::new(),
            }
        }

        pub fn spawn<F>(&mut self, future: F)
        where
            F: Future<Output = T> + Send + 'static,
        {
            self.pending.push_back(spawn(future));
        }

        /// Waits for the earliest-spawned task still in the set and returns its output, or
        /// `None` once the set is empty.
        pub async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
            let handle = self.pending.pop_front()?;
            Some(handle.await)
        }
    }
}
