// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = include_str!("../README.md")]

mod sync;

pub mod rados;

#[cfg(feature = "rbd")]
pub mod rbd;

#[cfg(feature = "cephfs")]
pub mod cephfs;

pub mod util;

pub mod error;

pub mod async_rt;

pub mod buf;

pub use error::{Error, Result};

/// Semver triple used for librados/librbd versioning and returned by [`rados::version`]/[`rbd::version`].
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LibVersion {
    /// The major version. This is incremented on ABI breaking changes.
    pub major: u32,
    /// The minor version.
    ///
    /// This is incremented on non-ABI changes, but may include breaking API changes. For example,
    /// librbd 1.15 and 1.17 are not API compatible, but linking an application built against the
    /// 1.15 headers to librbd 1.17 will not cause ABI breakage.
    pub minor: u32,
    /// The patch version, incremented on bug fixes.
    pub patch: u32,
}

impl std::fmt::Display for LibVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}
