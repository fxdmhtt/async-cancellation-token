//! # async-cancellation-token
//!
//! `async-cancellation-token` is a lightweight **single-threaded** Rust library that provides
//! **cancellation tokens** for cooperative cancellation of asynchronous tasks and callbacks.
//!
//! This crate is designed for **single-threaded async environments** such as `futures::executor::LocalPool`.
//! It internally uses `Rc`, `Cell`, and `RefCell`, and is **not thread-safe**.
//!
//! Features:
//! - `CancellationTokenSource` can cancel multiple associated `CancellationToken`s.
//! - `CancellationToken` can be awaited via `.cancelled()` or checked synchronously.
//! - Supports registration of **one-time callbacks** (`FnOnce`) that run on cancellation.
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
//! // Spawn a task that performs 5 steps but can be cancelled
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
//! // Cancel after 250ms
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
    fmt::{Debug, Display},
    future::Future,
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context, Poll, Waker},
};

use slab::Slab;

/// Inner shared state for `CancellationToken` and `CancellationTokenSource`.
///
/// This is the single-threaded shared state. All fields are internal and should not
/// be accessed directly outside the crate.
///
/// - `cancelled`: `true` once cancellation has occurred.
/// - `wakers`: list of wakers for async futures awaiting cancellation.
/// - `callbacks`: one-time callbacks (`FnOnce`) registered to run on cancellation.
///   These are stored in a `Slab` to allow stable keys for `CancellationTokenRegistration`.
#[derive(Default)]
struct Inner {
    /// Whether the token has been cancelled.
    cancelled: Cell<bool>,
    /// List of wakers to wake when cancellation occurs.
    wakers: RefCell<Vec<Waker>>,
    /// List of callbacks to call when cancellation occurs.
    callbacks: RefCell<Slab<Box<dyn FnOnce()>>>,
}

impl Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("cancelled", &self.cancelled)
            .field("wakers.len", &self.wakers.borrow().len())
            .field("callbacks.len", &self.callbacks.borrow().len())
            .finish()
    }
}

/// A source that can cancel associated `CancellationToken`s.
///
/// Cancellation is **cooperative** and single-threaded. When cancelled:
/// - All registered `FnOnce` callbacks are called (in registration order).
/// - All futures waiting via `CancellationToken::cancelled()` are woken.
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
#[derive(Debug, Default, Clone)]
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
#[derive(Debug, Clone)]
pub struct CancellationToken {
    inner: Rc<Inner>,
}

/// Error returned when a cancelled token is checked synchronously.
#[derive(Debug, Copy, Clone, Default, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Cancelled;

impl Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled by CancellationTokenSource")
    }
}

impl Error for Cancelled {}

impl CancellationTokenSource {
    /// Create a new `CancellationTokenSource`.
    pub fn new() -> Self {
        Default::default()
    }

    /// Get a `CancellationToken` associated with this source.
    pub fn token(&self) -> CancellationToken {
        CancellationToken {
            inner: self.inner.clone(),
        }
    }

    /// Cancel all associated tokens.
    ///
    /// This marks the source as cancelled. After cancellation:
    /// - All registered callbacks are called exactly once.
    /// - All waiting futures are woken.
    ///
    /// **Note:** Cancellation is **idempotent**; calling this method multiple times has no effect.
    /// **FnOnce callbacks will only be called once**.
    ///
    /// Single-threaded only. Not safe to call concurrently from multiple threads.
    pub fn cancel(&self) {
        if !self.inner.cancelled.replace(true) {
            // Call all registered callbacks
            for cb in self.inner.callbacks.borrow_mut().drain() {
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
    /// - If the token is **already cancelled**, the callback is called immediately.
    /// - Otherwise, the callback is stored and will be called exactly once upon cancellation.
    ///
    /// Returns a `CancellationTokenRegistration`, which will **remove the callback if dropped
    /// before cancellation**.
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
    /// let reg = token.register(move || {
    ///     flag_clone.set(true);
    /// });
    ///
    /// cts.cancel();
    /// assert!(flag.get());
    ///
    /// drop(reg);
    /// ```
    pub fn register(&self, f: impl FnOnce() + 'static) -> Option<CancellationTokenRegistration> {
        if self.is_cancelled() {
            f();
            None
        } else {
            CancellationTokenRegistration {
                inner: Rc::downgrade(&self.inner),
                key: self.inner.callbacks.borrow_mut().insert(Box::new(f)),
            }
            .into()
        }
    }
}

/// Represents a registered callback on a `CancellationToken`.
///
/// When this object is dropped **before the token is cancelled**, the callback
/// is automatically removed. If the token is already cancelled, Drop does nothing.
///
/// This ensures that callbacks are **only called once** and resources are cleaned up.
///
/// **Single-threaded only.** Not safe to use concurrently.
#[derive(Debug)]
pub struct CancellationTokenRegistration {
    inner: Weak<Inner>,
    key: usize,
}

impl Drop for CancellationTokenRegistration {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            if inner.cancelled.get() {
                // Callback was already removed
                return;
            }
            let _ = inner.callbacks.borrow_mut().remove(self.key);
        }
    }
}

/// A future that completes when a `CancellationToken` is cancelled.
///
/// - If the token is already cancelled, poll returns `Poll::Ready` immediately.
/// - Otherwise, the future registers its waker and returns `Poll::Pending`.
///
/// **Single-threaded only.** Not Send or Sync.
/// The future will be woken exactly once when the token is cancelled.
#[derive(Debug)]
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
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    use futures::{FutureExt, executor::LocalPool, pin_mut, select, task::LocalSpawnExt};
    use futures_timer::Delay;

    use super::*;

    /// Test cooperative cancellation of two tasks with different mechanisms.
    #[test]
    fn cancel_two_tasks() {
        let cancelled_a = Rc::new(Cell::new(false));
        let cancelled_b = Rc::new(Cell::new(false));

        let task_a = |token: CancellationToken| {
            let cancelled_a = Rc::clone(&cancelled_a);

            async move {
                for _ in 1..=5 {
                    let delay = Delay::new(Duration::from_millis(50)).fuse();
                    let cancelled = token.cancelled().fuse();

                    pin_mut!(delay, cancelled);

                    select! {
                        _ = delay => {},
                        _ = cancelled => {
                            cancelled_a.set(true);
                            break;
                        },
                    }
                }
            }
        };

        let task_b = |token: CancellationToken| {
            let cancelled_b = Rc::clone(&cancelled_b);

            async move {
                for _ in 1..=5 {
                    Delay::new(Duration::from_millis(80)).await;

                    if token.check_cancelled().is_err() {
                        cancelled_b.set(true);
                        break;
                    }
                }
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

        // Cancel after 200ms
        {
            let cts_clone = cts.clone();
            spawner
                .spawn_local(
                    async move {
                        Delay::new(Duration::from_millis(200)).await;
                        cts_clone.cancel();
                    }
                    .map(|_| ()),
                )
                .unwrap();
        }

        pool.run();

        // Cancelled flags should be set
        assert!(cts.is_cancelled());
        assert!(cancelled_a.get());
        assert!(cancelled_b.get());

        // Calling cancel again should not panic or change state
        cts.cancel();
        assert!(cts.is_cancelled());
    }

    /// Test registering callbacks before and after cancellation, including Drop behavior.
    #[test]
    fn cancellation_register_callbacks() {
        let cts = CancellationTokenSource::new();
        let token = cts.token();

        let flag_before = Rc::new(Cell::new(false));
        let flag_after = Rc::new(Cell::new(false));
        let flag_drop = Rc::new(Cell::new(false));

        // 1. Callback registered before cancel → should execute
        let reg_before = {
            let flag = Rc::clone(&flag_before);
            token
                .register(move || {
                    flag.set(true);
                })
                .unwrap()
        };

        cts.cancel();
        assert!(flag_before.get());

        drop(reg_before);

        // 2. Callback registered after cancel → executes immediately
        {
            let flag = Rc::clone(&flag_after);
            token.register(move || {
                flag.set(true);
            });
        }
        assert!(flag_after.get());

        // 3. Callback registered but dropped before cancel → should NOT execute
        let token2 = CancellationTokenSource::new().token();
        let reg_drop = {
            let flag = Rc::clone(&flag_drop);
            token2
                .register(move || {
                    flag.set(true);
                })
                .unwrap()
        };
        drop(reg_drop); // dropped before cancel
        token2.inner.cancelled.set(true); // force cancel
        assert!(!flag_drop.get());
    }

    /// Test that CancelledFuture returns Poll::Ready after cancellation
    #[test]
    fn cancelled_future_poll_ready() {
        let cts = CancellationTokenSource::new();
        let token = cts.token();
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();

        let finished = Rc::new(Cell::new(false));
        let finished_clone = Rc::clone(&finished);

        spawner
            .spawn_local(
                async move {
                    token.cancelled().await;
                    finished_clone.set(true);
                }
                .map(|_| ()),
            )
            .unwrap();

        // Cancel token
        cts.cancel();

        pool.run();
        assert!(finished.get());
    }

    /// Test multiple callbacks and idempotent cancellation
    #[test]
    fn multiple_callbacks_and_idempotent_cancel() {
        let cts = CancellationTokenSource::new();
        let token = cts.token();

        let flags: Vec<_> = (0..3).map(|_| Rc::new(Cell::new(false))).collect();

        let regs: Vec<_> = flags
            .iter()
            .map(|flag| {
                let f = Rc::clone(flag);
                token
                    .register(move || {
                        f.set(true);
                    })
                    .unwrap()
            })
            .collect();

        // Cancel once
        cts.cancel();
        for flag in &flags {
            assert!(flag.get());
        }

        // Cancel again → should not panic, flags remain true
        cts.cancel();
        for flag in &flags {
            assert!(flag.get());
        }

        drop(regs); // dropping after cancel → nothing happens, still safe
    }
}
