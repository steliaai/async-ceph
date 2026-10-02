// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#![doc = include_str!("../README.md")]
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

#[cfg(not(any(ceph_sys_bundled, docsrs)))]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

#[cfg(any(ceph_sys_bundled, docsrs))]
include!("bindings.rs");

/// Compile-time list of librbd feature names mentioned in LIBRBD_SUPPORTS_* macros.
///
/// To easily check if a particular feature is available at compile time, use the [`supports`]
/// function. This list is provided in case more complex logic is required.
pub const LIBRBD_SUPPORTS: &[&str] = _LIBRBD_SUPPORTS;

/// Check if a particular librbd feature is supported by the generated bindings.
///
/// The feature name is what comes after the LIBRBD_SUPPORTS_ prefix in the support macros defined
/// in `librbd.h`. For example, the feature name for `LIBRBD_SUPPORTS_AIO_FLUSH` is `AIO_FLUSH`.
pub const fn supports(feature: &str) -> bool {
    _supports(feature)
}
