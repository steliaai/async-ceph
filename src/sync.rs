// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg_attr(loom, path = "sync/loom.rs")]
#[cfg_attr(not(loom), path = "sync/non_loom.rs")]
mod inner;

#[doc(inline)]
pub use inner::*;
