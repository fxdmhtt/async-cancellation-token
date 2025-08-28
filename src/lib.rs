//! # async-cancellation-token
//!
//! `async-cancellation-token` is a lightweight **single-threaded** Rust library that provides
//! **cancellation tokens** for cooperative cancellation of asynchronous tasks and callbacks.
//!
//! This crate works in single-threaded async environments (e.g., `futures::executor::LocalPool`)
//! and uses `Rc`, `Cell`, and `RefCell` internally. It is **not thread-safe**.
//!
//! ## Example
//!
//! ```rust
//! use std::time::Duration;
//! use async_cancellation_token::CancellationTokenSource;
//! use futures::{FutureExt, executor::LocalPool, pin_mut, select, task::LocalSpawnExt};
//! use futures_timer::Delay;
//!
//! let cts = CancellationTokenSource::new();
//! let token = cts.token();
//!
//! let mut pool = LocalPool::new();
//! let spawner = pool.spawner();
//!
//! spawner.spawn_local(async move {
//!     for i in 1..=5 {
//!         let delay = Delay::new(Duration::from_millis(100)).fuse();
//!         let cancelled = token.cancelled().fuse();
//!         pin_mut!(delay, cancelled);
//!
//!         select! {
//!             _ = delay => println!("Step {i}"),
//!             _ = cancelled => {
//!                 println!("Cancelled!");
//!                 break;
//!             }
//!         }
//!     }
//! }.map(|_| ())).unwrap();
//!
//! spawner.spawn_local(async move {
//!     Delay::new(Duration::from_millis(250)).await;
//!     cts.cancel();
//! }.map(|_| ())).unwrap();
//!
//! pool.run();
//! ```

use std::{
    cell::{Cell, RefCell},
    error::Error,
    fmt::Display,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

/// Inner shared state for `CancellationToken` and `CancellationTokenSource`.
#[derive(Default)]
struct Inner {
    /// Whether the token has been cancelled.
    cancelled: Cell<bool>,
    /// List of wakers to wake when cancellation occurs.
    wakers: RefCell<Vec<Waker>>,
    /// List of callbacks to call when cancellation occurs.
    callbacks: RefCell<Vec<Box<dyn FnOnce()>>>,
}

/// A source that can cancel associated `CancellationToken`s.
///
/// # Example
///
/// ```rust
/// use async_cancellation_token::CancellationTokenSource;
///
/// let cts = CancellationTokenSource::new();
/// let token = cts.token();
///
/// assert!(!cts.is_cancelled());
/// cts.cancel();
/// assert!(cts.is_cancelled());
/// ```
#[derive(Clone)]
pub struct CancellationTokenSource {
    inner: Rc<Inner>,
}

/// A token that can be checked for cancellation or awaited.
///
/// # Example
///
/// ```rust
/// use async_cancellation_token::CancellationTokenSource;
/// use futures::{FutureExt, executor::LocalPool, task::LocalSpawnExt};
///
/// let cts = CancellationTokenSource::new();
/// let token = cts.token();
///
/// let mut pool = LocalPool::new();
/// pool.spawner().spawn_local(async move {
///     token.cancelled().await;
///     println!("Cancelled!");
/// }.map(|_| ())).unwrap();
///
/// cts.cancel();
/// pool.run();
/// ```
#[derive(Clone)]
pub struct CancellationToken {
    inner: Rc<Inner>,
}

/// Error returned when a cancelled token is checked synchronously.
#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Cancelled;

impl Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled by CancellationTokenSource")
    }
}

impl Error for Cancelled {}

impl Default for CancellationTokenSource {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationTokenSource {
    /// Create a new `CancellationTokenSource`.
    pub fn new() -> Self {
        Self {
            inner: Rc::new(Inner::default()),
        }
    }

    /// Get a `CancellationToken` associated with this source.
    pub fn token(&self) -> CancellationToken {
        CancellationToken {
            inner: self.inner.clone(),
        }
    }

    /// Cancel all associated tokens.
    ///
    /// This triggers any registered callbacks and wakes all wakers.
    pub fn cancel(&self) {
        if !self.inner.cancelled.replace(true) {
            // Call all registered callbacks
            for cb in self.inner.callbacks.borrow_mut().drain(..) {
                cb();
            }

            // Wake all tasks waiting for cancellation
            for w in self.inner.wakers.borrow_mut().drain(..) {
                w.wake();
            }
        }
    }

    /// Check if this source has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.get()
    }
}

impl CancellationToken {
    /// Check if the token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.get()
    }

    /// Synchronously check cancellation and return `Err(Cancelled)` if cancelled.
    pub fn check_cancelled(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    /// Returns a `Future` that completes when the token is cancelled.
    ///
    /// # Example
    ///
    /// ```rust
    /// use async_cancellation_token::CancellationTokenSource;
    /// use futures::{FutureExt, executor::LocalPool, task::LocalSpawnExt};
    ///
    /// let cts = CancellationTokenSource::new();
    /// let token = cts.token();
    ///
    /// let mut pool = LocalPool::new();
    /// pool.spawner().spawn_local(async move {
    ///     token.cancelled().await;
    ///     println!("Cancelled!");
    /// }.map(|_| ())).unwrap();
    ///
    /// cts.cancel();
    /// pool.run();
    /// ```
    pub fn cancelled(&self) -> CancelledFuture {
        CancelledFuture {
            token: self.clone(),
        }
    }

    /// Register a callback to run when the token is cancelled.
    ///
    /// If the token is already cancelled, the callback is called immediately.
    ///
    /// # Example
    ///
    /// ```rust
    /// use std::{cell::Cell, rc::Rc};
    /// use async_cancellation_token::CancellationTokenSource;
    ///
    /// let cts = CancellationTokenSource::new();
    /// let token = cts.token();
    ///
    /// let flag = Rc::new(Cell::new(false));
    /// let flag_clone = Rc::clone(&flag);
    ///
    /// token.register(move || {
    ///     flag_clone.set(true);
    /// });
    ///
    /// cts.cancel();
    /// assert!(flag.get());
    /// ```
    pub fn register(&self, f: impl FnOnce() + 'static) {
        if self.is_cancelled() {
            f();
        } else {
            self.inner.callbacks.borrow_mut().push(Box::new(f));
        }
    }
}

/// Future that completes when a `CancellationToken` is cancelled.
pub struct CancelledFuture {
    token: CancellationToken,
}

impl Future for CancelledFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.token.is_cancelled() {
            Poll::Ready(())
        } else {
            let mut wakers = self.token.inner.wakers.borrow_mut();
            if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
                wakers.push(cx.waker().clone());
            }
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::{FutureExt, executor::LocalPool, pin_mut, select, task::LocalSpawnExt};
    use futures_timer::Delay;

    use super::*;

    #[test]
    fn cancel_two_tasks() {
        let cancelled_a = Rc::new(Cell::new(false));
        let cancelled_b = Rc::new(Cell::new(false));

        let task_a = |token: CancellationToken| {
            let cancelled_a = Rc::clone(&cancelled_a);

            async move {
                println!("Task A started");

                for i in 1..=10 {
                    let delay = Delay::new(Duration::from_millis(300)).fuse();
                    let cancelled = token.cancelled().fuse();

                    pin_mut!(delay, cancelled);

                    select! {
                        _ = delay => {
                            println!("Task A step {i}");
                        },
                        _ = cancelled => {
                            println!("Task A detected cancellation, cleaning up...");
                            // Cleanup && Dispose
                            cancelled_a.set(true);
                            break;
                        },
                    }
                }

                println!("Task A finished");
            }
        };

        let task_b = |token: CancellationToken| {
            let cancelled_b = Rc::clone(&cancelled_b);

            async move {
                println!("Task B started");

                for i in 1..=10 {
                    Delay::new(Duration::from_millis(500)).await;

                    println!("Task B step {i}");
                    if token.check_cancelled().is_err() {
                        println!("Task B noticed cancellation after step {i}");
                        // Cleanup && Dispose
                        cancelled_b.set(true);
                        break;
                    }
                }

                println!("Task B finished");
            }
        };

        let cts = CancellationTokenSource::new();

        let mut pool = LocalPool::new();
        let spawner = pool.spawner();

        spawner
            .spawn_local(task_a(cts.token()).map(|_| ()))
            .unwrap();
        spawner
            .spawn_local(task_b(cts.token()).map(|_| ()))
            .unwrap();

        {
            let cts = cts.clone();
            spawner
                .spawn_local(
                    async move {
                        Delay::new(Duration::from_secs(2)).await;
                        println!("Cancelling all tasks!");
                        cts.cancel();
                    }
                    .map(|_| ()),
                )
                .unwrap();
        }

        pool.run();

        assert!(cts.is_cancelled());
        assert!(cancelled_a.get());
        assert!(cancelled_b.get());
    }

    #[test]
    fn cancellation_register_callbacks() {
        let cts = CancellationTokenSource::new();
        let token = cts.token();

        let flag1 = Rc::new(Cell::new(false));
        let flag2 = Rc::new(Cell::new(false));

        {
            let flag1 = Rc::clone(&flag1);
            token.register(move || {
                flag1.set(true);
            });
        }

        cts.cancel();
        assert!(flag1.get());

        {
            let flag2 = Rc::clone(&flag2);
            token.register(move || {
                flag2.set(true);
            });
        }

        assert!(flag2.get());
    }
}
