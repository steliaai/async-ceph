// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use core::{fmt, task::Waker};

/// Mock futures_util::task::AtomicWaker.
///
/// We rely on loom's mock for Tokio's `AtomicWaker`, but must adapt its API
/// to match that of futures.
pub struct AtomicWaker(loom::future::AtomicWaker);

impl AtomicWaker {
    /// Create an `AtomicWaker`.
    pub fn new() -> Self {
        Self(loom::future::AtomicWaker::new())
    }

    pub fn register(&self, waker: &Waker) {
        self.0.register(waker.clone());
    }

    pub fn wake(&self) {
        self.0.wake();
    }

    pub fn take(&self) -> Option<Waker> {
        self.0.take_waker()
    }
}

impl Default for AtomicWaker {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for AtomicWaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AtomicWaker")
    }
}
