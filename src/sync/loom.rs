// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(unused)]

pub use loom::{cell, hint, sync::*, thread};

pub mod futures;

#[cfg(test)]
pub mod test {
    pub fn block_on<F: Future>(future: F) -> F::Output {
        loom::future::block_on(future)
    }
}
