// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    ffi::c_void,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use futures_util::FutureExt;
use rbd_sys::librbd_progress_fn_t;

use crate::async_rt::{DynExecutor, Executor};

/// Progress update from a `_with_progress` librbd API.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// The amount of data that has been processed, in arbitrary units.
    pub offset: u64,
    /// The total amount of data that needs processing, in arbitrary units.
    ///
    /// Note that `total` may be zero, in which case the operation has not started yet.
    pub total: u64,
}

impl Progress {
    /// Return progress as real number from 0 to 1.
    pub fn as_fraction(&self) -> f64 {
        if self.started() {
            self.offset as f64 / self.total as f64
        } else {
            0.0
        }
    }

    /// Return progress as a percentage, in units of 1%.
    pub fn as_percentage(&self) -> u64 {
        if self.started() { 100 * self.offset / self.total } else { 0 }
    }

    /// Whether the operation has started.
    pub fn started(&self) -> bool {
        self.total != 0
    }

    /// Whether the operation is completed.
    ///
    /// Note that this does not necessarily imply that the operation completed *successfully*.
    /// Even an operation that fails immediately will receive a progress update with `.completed() == true`.
    pub fn completed(&self) -> bool {
        self.started() && self.offset == self.total
    }
}

/// Type that can receive progress updates from a librbd operation.
pub trait ProgressWatcher: Send + 'static {
    /// Called when the operation's progress is updated.
    fn on_progress(this: Pin<&mut Self>, progress: Progress);
}

impl<F: FnMut(Progress) + Send + 'static + Unpin> ProgressWatcher for F {
    fn on_progress(mut this: Pin<&mut Self>, progress: Progress) {
        this(progress)
    }
}

enum ProgressOpState<'a, R: Send + 'static> {
    Created {
        sync_op: Box<dyn FnOnce(librbd_progress_fn_t, *mut c_void) -> R + Send + 'static>,
        executor: &'a dyn DynExecutor,
    },
    Submitted(<&'a dyn DynExecutor as Executor>::JoinHandle<R>),
    Poisoned, // Panicked during spawn_blocking
}

/// [`Future`] that wraps a librados/rbd `_with_progress` function call.
///
/// Provides an optional [`with_progress`](ProgressOp::with_progress) combinator that can be used to
/// listen to progress updates as the operation completes.
#[must_use = "ProgressOp is a future and does nothing unless `.await`ed or polled"]
pub struct ProgressOp<'a, R: Send + 'static>(ProgressOpState<'a, R>);

impl<'a, R: Send + 'static> Future for ProgressOp<'a, R> {
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let state = &mut Pin::into_inner(self).0;

        match state {
            ProgressOpState::Poisoned => panic!("ProgressOpState poisoned"),
            ProgressOpState::Created { .. } => {
                let ProgressOpState::Created { sync_op, executor } = std::mem::replace(state, ProgressOpState::Poisoned) else {
                    unreachable!()
                };
                let handle = executor.spawn_blocking(move || (sync_op)(Some(Self::noop_cb), std::ptr::null_mut()));
                *state = ProgressOpState::Submitted(handle);
            }
            _ => (),
        }
        let ProgressOpState::Submitted(handle) = state else {
            unreachable!()
        };

        handle.poll_unpin(cx).map(|res| res.expect("failed to await blocking task"))
    }
}

impl<'a, R: Send + 'static> ProgressOp<'a, R> {
    pub(crate) fn new(
        executor: &'a dyn DynExecutor,
        sync_op: impl FnOnce(librbd_progress_fn_t, *mut c_void) -> R + Send + 'static,
    ) -> Self {
        ProgressOp(ProgressOpState::Created {
            sync_op: Box::new(sync_op),
            executor,
        })
    }

    /// Add a [`ProgressWatcher`] to this librbd operation.
    ///
    /// This can be a callback function in the form of a [`FnMut`] closure taking a [`Progress`] value.
    /// Note that it must be [`Send`], [`Unpin`] and `'static`.
    ///
    /// If running the operation asynchronously or on another thread, the [`AtomicProgressWatcher`] type can be used
    /// to fetch progress information concurrently.
    ///
    /// # Panics
    /// If called on a [`ProgressOp`] that was polled.
    pub fn with_progress<P: ProgressWatcher>(self, watch: P) -> impl Future<Output = R> + Send + 'a {
        let ProgressOpState::Created { sync_op, executor } = self.0 else {
            panic!("cannot call with_progress on ProgressOp after it has been polled");
        };

        #[pin_project::pin_project]
        struct CbData<P: ProgressWatcher> {
            #[pin]
            watch: P,
            total: u64,
        }

        unsafe extern "C" fn callback<P: ProgressWatcher>(offset: u64, total: u64, arg: *mut c_void) -> i32 {
            // SAFETY: value is pinned to the stack of the sync operation until it completes
            let data = unsafe { Pin::new_unchecked(&mut *(arg as *mut CbData<P>)) };
            let data = data.project();
            *data.total = total;
            ProgressWatcher::on_progress(data.watch, Progress { offset, total });
            0
        }

        async move {
            let handle = executor.spawn_blocking(move || {
                let data = std::pin::pin!(CbData { watch, total: 0 });
                let ret = (sync_op)(Some(callback::<P>), &raw const *data as *mut _);

                // librbd doesn't send a "completed" progress update, so we do it ourselves here
                let data = data.project();
                let total = 1.max(*data.total);
                ProgressWatcher::on_progress(data.watch, Progress { offset: total, total });
                ret
            });
            handle.await.expect("failed to await blocking task")
        }
    }

    /// Specify a callback that will receive progress updates to this librbd operation.
    ///
    /// This is an alias for [`with_progress`](Self::with_progress) to help with closure type inference.
    ///
    /// # Panics
    /// If called on a [`ProgressOp`] that was polled.
    pub fn with_progress_cb<F>(self, callback: F) -> impl Future<Output = R> + Send + 'a
    where
        F: FnMut(Progress) + Send + Unpin + 'static, {
        self.with_progress(callback)
    }

    unsafe extern "C" fn noop_cb(_offset: u64, _total: u64, _arg: *mut c_void) -> i32 {
        0
    }
}

#[derive(Debug, Default)]
struct AtomicProgress {
    offset: AtomicU64,
    total: AtomicU64,
}

impl AtomicProgress {
    fn update(&self, progress: Progress) {
        self.total.store(progress.total, Ordering::Relaxed);
        self.offset.store(progress.offset, Ordering::Release);
    }

    fn latest(&self) -> Progress {
        let offset = self.offset.load(Ordering::Acquire);
        let total = self.total.load(Ordering::Relaxed);
        Progress { offset, total }
    }
}

/// Provides shared access to the latest progress information from a librbd operation that returns a
/// [`ProgressOp`].
///
/// Clone it and pass it to [`ProgressOp::with_progress`] to concurrently read the latest progress information.
#[derive(Debug, Clone, Default)]
pub struct AtomicProgressWatcher(Arc<AtomicProgress>);

impl AtomicProgressWatcher {
    /// Creates a [`AtomicProgressWatcher`] value.
    ///
    /// Clone it and pass it to [`ProgressOp::with_progress`] so that [`Self::latest`]
    /// returns the up-to-date progress information.
    ///
    /// Beware that passing clones to concurrent operations will lead to meaningless data.
    pub fn new() -> Self {
        Default::default()
    }

    /// Get the most recent progress update.
    ///
    /// Note that updates will only sync if `self` is cloned and passed to [`ProgressOp::with_progress`]
    /// first.
    pub fn latest(&self) -> Progress {
        self.0.latest()
    }
}

impl ProgressWatcher for AtomicProgressWatcher {
    fn on_progress(this: Pin<&mut Self>, progress: Progress) {
        this.0.update(progress);
    }
}
