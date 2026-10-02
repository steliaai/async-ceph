// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(unused)]

pub use std::{cell, hint, sync::*, thread};

pub mod futures {
    pub use futures_channel::oneshot;
    pub use futures_util::task::AtomicWaker;
}

#[cfg(test)]
pub mod test {
    #[cfg(feature = "tokio_rt")]
    pub fn block_on<F: Future>(future: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(future)
    }
}
