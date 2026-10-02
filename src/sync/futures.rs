// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Re-implementation of primitives from the `futures` crate with loom types.

mod atomic_waker;
mod lock;
pub mod oneshot;

pub use atomic_waker::AtomicWaker;
