// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Required abstractions over a Rust async runtime equipped with a thread pool for
//! spawning blocking work.
//!
//! This is principally modeled through the [`Executor`] trait, which is mainly based
//! on the Tokio runtime model. However, it can be implemented by other async runtimes as well.
//!
//! To avoid generics bubbling up to every type in the crate, a dyn-compatible shim trait called
//! [`DynExecutor`] is also available. It is not meant to be used directly; all [`Sized`] executors
//! implement it, and `dyn DynExecutor` itself implements `Executor`.
//!
//! When Tokio is used, you can enable the `tokio_rt` crate feature to access the [`Tokio`] marker
//! type which implements [`Executor`] by deferring to the current runtime. [`Executor`] is also
//! implemented on [`tokio::runtime::Runtime`] directly.
//!
//! For other executors, you will want to implement the [`Executor`] trait yourself (either on a
//! concrete executor or a marker type) and pass it when constructing the
//! [`RadosClient`](crate::rados::RadosClient).

use std::{
    any::Any,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use futures_util::FutureExt;

/// Failure to await a task to completion.
#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    /// The task was aborted through [`JoinHandle::abort`].
    #[error("joined task cancelled")]
    Cancelled,
    /// The task panicked.
    #[error("joined task panicked")]
    // not including the panic payload so this error type is Sync
    // tokio JoinError panic payloads are only Send
    Panic,
    /// A runtime-specific error caused the task to fail.
    #[error("joined task failed: {0}")]
    Other(String),
}

/// An awaitable handle that can be used to wait for a spawned task.
///
/// Dropping this handle should not cancel the task; it should instead detach it.
pub trait JoinHandle<T>: Future<Output = Result<T, JoinError>> {
    /// Tell the async runtime that the task should be aborted; i.e. stop being polled.
    ///
    /// If the task had not completed by this time the abort was processed, awaiting this
    /// [`JoinHandle`] will return [`JoinError::Cancelled`].
    ///
    /// Note that tasks spawned through [`Executor::spawn_blocking`] typically cannot be aborted.
    fn abort(&self);
}

/// An async runtime that can spawn and wait on futures.
pub trait Executor {
    /// A handle that can be used to await or abort a spawned task.
    type JoinHandle<T: Send + 'static>: JoinHandle<T> + Send + 'static;

    /// Spawn an asynchronous task, returning a handle to it.
    fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static;

    /// Run blocking code in a threadpool, returning a awaitable handle to it.
    fn spawn_blocking<F, R>(&self, f: F) -> Self::JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static;
}

impl<E: Executor + ?Sized> Executor for &E {
    type JoinHandle<T: Send + 'static> = E::JoinHandle<T>;

    fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static, {
        (*self).spawn(future)
    }

    fn spawn_blocking<F, R>(&self, f: F) -> Self::JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static, {
        (*self).spawn_blocking(f)
    }
}

/// A boxed type-erased value that is [`Send`] and `'static`.
pub type Erased = Box<dyn Any + Send>;

/// A boxed [`JoinHandle`] trait object with a type-erased return value.
pub type DynJoinHandle = Pin<Box<dyn JoinHandle<Erased> + Send + 'static>>;

/// Dyn-compatible (object safe) version of the [`Executor`] trait.
///
/// Any [`Sized`] type that implements [`Executor`] also implements this trait, so it does not have to be
/// done manually.
///
/// Furthermore, the `dyn DynExecutor` type itself implements [`Executor`], so the
/// [`spawn`][Executor::spawn] and [`spawn_blocking`][Executor::spawn_blocking] methods can be called on it.
pub trait DynExecutor: Sync {
    /// Spawn a future with a type-erased return value as an asynchronous task, returning a handle to it.
    ///
    /// This method is part of a "dyn bridge" and should not be called directly, since [`Executor::spawn`]
    /// can be called directly on a `dyn DynExecutor`.
    fn dyn_spawn(&self, future: Pin<Box<dyn Future<Output = Erased> + Send + 'static>>) -> DynJoinHandle;

    /// Run blocking code with a type-erased return value in a threadpool, returning a handle to it.
    ///
    /// This method is part of a "dyn bridge" and should not be called directly, since [`Executor::spawn_blocking`]
    /// can be called directly on a `dyn DynExecutor`.
    fn dyn_spawn_blocking(&self, f: Box<dyn FnOnce() -> Erased + Send + 'static>) -> DynJoinHandle;

    /// Get the type name of the concrete [`Executor`] this [`DynExecutor`] was created from.
    ///
    /// Used for debug purposes.
    fn concrete_type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

impl<E: Executor + Sync> DynExecutor for E {
    fn dyn_spawn(&self, future: Pin<Box<dyn Future<Output = Erased> + Send + 'static>>) -> DynJoinHandle {
        Box::pin(self.spawn(future))
    }

    fn dyn_spawn_blocking(&self, f: Box<dyn FnOnce() -> Erased + Send + 'static>) -> DynJoinHandle {
        Box::pin(self.spawn_blocking(f))
    }
}

/// Wrapper around a [`DynJoinHandle`] that downcasts the type-erased return value.
pub struct JoinHandleBridge<T: Send + 'static> {
    inner: DynJoinHandle,
    phantom: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> Future for JoinHandleBridge<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.as_mut().poll(cx).map_ok(|ret| *ret.downcast().expect("type mismatch"))
    }
}

impl<T: Send + 'static> JoinHandle<T> for JoinHandleBridge<T> {
    fn abort(&self) {
        self.inner.abort();
    }
}

impl Executor for dyn DynExecutor + '_ {
    type JoinHandle<T: Send + 'static> = JoinHandleBridge<T>;

    fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static, {
        let boxed = Box::pin(future.map(|ret| Box::new(ret) as Erased));
        JoinHandleBridge {
            inner: self.dyn_spawn(boxed),
            phantom: PhantomData,
        }
    }

    fn spawn_blocking<F, R>(&self, f: F) -> Self::JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static, {
        let boxed = Box::new(move || Box::new(f()) as Erased);
        JoinHandleBridge {
            inner: self.dyn_spawn_blocking(boxed),
            phantom: PhantomData,
        }
    }
}

impl std::fmt::Debug for dyn DynExecutor + '_ {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "&{}", self.concrete_type_name())
    }
}

#[cfg(feature = "tokio_rt")]
mod tokio_impl {
    use super::Executor;
    use std::{pin::Pin, task::Poll};

    pub struct TokioJoinHandle<T>(tokio::task::JoinHandle<T>);

    impl<T> Unpin for TokioJoinHandle<T> {}

    impl From<tokio::task::JoinError> for super::JoinError {
        fn from(value: tokio::task::JoinError) -> Self {
            if value.is_cancelled() {
                super::JoinError::Cancelled
            } else if value.is_panic() {
                super::JoinError::Panic
            } else {
                super::JoinError::Other(value.to_string())
            }
        }
    }

    impl<T> Future for TokioJoinHandle<T> {
        type Output = Result<T, super::JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
            match Pin::new(&mut self.0).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(v)) => Poll::Ready(Ok(v)),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
            }
        }
    }

    impl<T> super::JoinHandle<T> for TokioJoinHandle<T> {
        fn abort(&self) {
            self.0.abort();
        }
    }

    /// Marker type for the current tokio runtime context.
    ///
    /// Implements [`Executor`], deferring to the [`tokio::task::spawn`] and
    /// [`tokio::task::spawn_blocking`] functions.
    pub struct Tokio;

    impl Executor for Tokio {
        type JoinHandle<T: Send + 'static> = TokioJoinHandle<T>;

        fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static, {
            TokioJoinHandle(tokio::spawn(future))
        }

        fn spawn_blocking<F, R>(&self, f: F) -> Self::JoinHandle<R>
        where
            F: FnOnce() -> R + Send + 'static,
            R: Send + 'static, {
            TokioJoinHandle(tokio::task::spawn_blocking(f))
        }
    }

    impl Executor for tokio::runtime::Runtime {
        type JoinHandle<T: Send + 'static> = TokioJoinHandle<T>;

        fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static, {
            TokioJoinHandle(self.spawn(future))
        }

        fn spawn_blocking<F, R>(&self, f: F) -> Self::JoinHandle<R>
        where
            F: FnOnce() -> R + Send + 'static,
            R: Send + 'static, {
            TokioJoinHandle(self.spawn_blocking(f))
        }
    }
}

#[cfg(feature = "tokio_rt")]
#[doc(inline)]
pub use tokio_impl::{Tokio, TokioJoinHandle};
