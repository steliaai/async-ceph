// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{fmt::Debug, os::raw::c_void, ptr::NonNull};

use rados_sys::rados_ioctx_destroy;

use crate::{async_rt::DynExecutor, rados::RadosClientHandle, util::ArcWait};

#[derive(Debug)]
pub(crate) struct IoCtxPtr {
    #[allow(unused)]
    cluster: RadosClientHandle,
    ioctx: NonNull<c_void>,
}

unsafe impl Send for IoCtxPtr {}
unsafe impl Sync for IoCtxPtr {}

impl IoCtxPtr {
    pub unsafe fn new(cluster: RadosClientHandle, ioctx: NonNull<c_void>) -> Self {
        Self { cluster, ioctx }
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.ioctx.as_ptr()
    }
}

impl Drop for IoCtxPtr {
    #[tracing::instrument("IoCtxPtr::drop", level = "debug")]
    fn drop(&mut self) {
        unsafe { rados_ioctx_destroy(self.as_ptr()) };
    }
}

/// Refcounted owned handle to an [`IoCtx`].
pub(crate) type IoCtxHandle = ArcWait<IoCtxPtr>;

/// Safe wrapper around a librados [`rados_ioctx_t`](https://docs.ceph.com/en/latest/rados/api/librados/#c.rados_ioctx_t).
pub struct IoCtx<'a> {
    pub(crate) handle: IoCtxHandle,
    #[allow(unused)] // for now until we implement rados APIs
    pub(crate) executor: &'a dyn DynExecutor,
}

unsafe impl Send for IoCtx<'_> {}
unsafe impl Sync for IoCtx<'_> {}

impl Debug for IoCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoCtx").field("handle", &self.handle.as_ptr()).finish()
    }
}

impl<'a> IoCtx<'a> {
    pub(crate) fn new(handle: IoCtxHandle, executor: &'a dyn DynExecutor) -> Self {
        Self { handle, executor }
    }
}
