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
