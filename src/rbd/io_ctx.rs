// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Librdb operations that operate on an IoCtx.

use std::{borrow::Borrow, ffi::c_void};

use rbd_sys::{librbd_progress_fn_t, rados_ioctx_t};
use tracing::Span;

use crate::{
    Error,
    Result,
    async_rt::Executor,
    rados::IoCtx,
    rbd::{features::Features, metadata::ImageSpec, options::ImageOptions, progress::ProgressOp, snap::SnapKey},
    util::{TryIntoCString, check_os_error, get_known_size_arr},
};

// To make doclinks more terse
#[allow(unused_imports)]
use std::ffi::CString;

impl<'a> IoCtx<'a> {
    async fn do_op<F, R>(&self, span: Span, f: F) -> R
    where
        F: FnOnce(rados_ioctx_t) -> R + Send + 'static,
        R: Send + 'static, {
        let ioctx = self.handle.clone();

        let handle = self.executor.spawn_blocking(move || {
            let _enter = span.enter();
            f(ioctx.as_ptr())
        });

        handle.await.expect("failed to await blocking task")
    }

    fn do_progress_op<F, R>(&self, span: Span, f: F) -> ProgressOp<'_, R>
    where
        F: FnOnce(rados_ioctx_t, librbd_progress_fn_t, *mut c_void) -> R + Send + 'static,
        R: Send + 'static, {
        let ioctx = self.handle.clone();
        ProgressOp::new(self.executor, move |cb, data| {
            let _enter = span.enter();
            f(ioctx.as_ptr(), cb, data)
        })
    }

    /// List images present in the RBD pool associated with this ioctx.
    pub async fn rbd_list(&self) -> Result<Vec<ImageSpec>> {
        let span = tracing::debug_span!("IoCtx::rbd_list");

        self.do_op(span, move |ioctx| unsafe {
            // SAFETY:
            // - cast is safe since ImageSpec is repr(transparent) with rbd_image_spec_t
            // - librbd properly synchronizes this operation
            get_known_size_arr(|ptr: *mut ImageSpec, size| rbd_sys::rbd_list2(ioctx, ptr.cast(), size), 64).map_err(Into::into)
        })
        .await
    }

    /// Creates a RBD image of `size` bytes under `name`.
    ///
    /// Fails if converting `name` to a [`CString`] fails.
    pub async fn rbd_create(&self, name: impl TryIntoCString, size: u64) -> Result<()> {
        let name = name.try_into_cstring().map_err(Error::ffi)?;
        let span = tracing::debug_span!("IoCtx::rbd_create", ?name);

        self.do_op(span, move |ioctx| unsafe {
            let mut order = 0;
            check_os_error(rbd_sys::rbd_create(ioctx, name.as_ptr(), size, &mut order))?;
            Ok(())
        })
        .await
    }

    /// Creates a RBD image of `size` bytes under `name` with the provided RBD feature flags.
    ///
    /// Fails if converting `name` to a [`CString`] fails.
    pub async fn rbd_create_with_features(&self, name: impl TryIntoCString, size: u64, features: Features) -> Result<()> {
        let name = name.try_into_cstring().map_err(Error::ffi)?;
        let span = tracing::debug_span!("IoCtx::rbd_create_with_features", ?name, size);

        self.do_op(span, move |ioctx| unsafe {
            let mut order = 0;
            check_os_error(rbd_sys::rbd_create2(ioctx, name.as_ptr(), size, features.bits(), &mut order))?;
            Ok(())
        })
        .await
    }

    /// Creates a RBD image of `size` bytes under `name` with the provided RBD image options.
    ///
    /// Returns the [`ImageOptions`] regardless of success/failure so that they can be reused.
    ///
    /// Fails if converting `name` to a [`CString`] fails.
    pub async fn rbd_create_with_options(
        &self,
        name: impl TryIntoCString,
        size: u64,
        options: impl Borrow<ImageOptions>,
    ) -> Result<()> {
        let name = name.try_into_cstring().map_err(Error::ffi);
        let opts = options.borrow();

        let span = tracing::debug_span!("IoCtx::rbd_create_with_options", ?name, size, ?opts);

        let name = name?;
        let sys_opts = opts.as_sys_repr()?;

        self.do_op(span, move |ioctx| unsafe {
            check_os_error(rbd_sys::rbd_create4(ioctx, name.as_ptr(), size, sys_opts.as_ptr()))
                .map(|_| ())
                .map_err(Into::into)
        })
        .await
    }

    /// Clone a RBD image from an image snapshot within this pool.
    ///
    /// The snapshot is created in `dest_pool`. To create a snapshot in the same pool as the original image,
    /// set `dest_pool` to `self` or use the [`rbd_clone_within`](Self::rbd_clone_within) shorthand.
    ///
    /// The snapshot **must** be protected via [`Snapshot::protect`](super::Snapshot::protect)
    /// for this to succeed; an error with [`ErrorKind::InvalidInput`](std::io::ErrorKind::InvalidInput)
    /// will be returned otherwise.
    ///
    /// The [`ImageOptions`] used for the destination image is returned once the operation is completed.
    ///
    /// Fails if converting `src_name`, `src_snap` or `dest_name` to a [`CString`] fails.
    pub async fn rbd_clone(
        &self,
        src_image: impl TryIntoCString,
        src_snap: impl TryIntoCString,
        dest_pool: &IoCtx<'_>,
        dest_name: impl TryIntoCString,
        dest_options: impl Borrow<ImageOptions>,
    ) -> Result<()> {
        let src_snap = src_snap.try_into_cstring().map_err(Error::ffi)?;
        self.rbd_clone_internal(src_image, src_snap, dest_pool, dest_name, dest_options).await
    }

    /// Clone a RBD image from an image snapshot within this pool, creating it in the same pool.
    ///
    /// To create a snapshot in a different pool, use [`rbd_clone`](Self::rbd_clone) instead.
    ///
    /// The snapshot **must** be protected via [`Snapshot::protect`](super::Snapshot::protect)
    /// for this to succeed; an error with [`ErrorKind::InvalidInput`](std::io::ErrorKind::InvalidInput)
    /// will be returned otherwise.
    ///
    /// The [`ImageOptions`] used for the destination image is returned once the operation is completed.
    ///
    /// Fails if converting `src_name`, `src_snap` or `dest_name` to a [`CString`] fails.
    pub async fn rbd_clone_within(
        &self,
        src_image: impl TryIntoCString,
        src_snap: impl TryIntoCString,
        dest_name: impl TryIntoCString,
        dest_options: impl Borrow<ImageOptions>,
    ) -> Result<()> {
        self.rbd_clone(src_image, src_snap, self, dest_name, dest_options).await
    }

    /// Clone a RBD image from an image snapshot within this pool.
    ///
    /// The snapshot is created in `dest_pool`. To create a snapshot in the same pool as the original image,
    /// set `dest_pool` to `self` or use the [`rbd_clone_within`](Self::rbd_clone_within) shorthand.
    ///
    /// The snapshot **must** be protected via [`Snapshot::protect`](super::Snapshot::protect)
    /// for this to succeed; an error with [`ErrorKind::InvalidInput`](std::io::ErrorKind::InvalidInput)
    /// will be returned otherwise.
    ///
    /// The [`ImageOptions`] used for the destination image is returned once the operation is completed.
    ///
    /// Fails if converting `src_name` or `dest_name` to a [`CString`] fails.
    #[cfg(feature = "rbd_v1-20")]
    pub async fn rbd_clone_by_key(
        &self,
        src_image: impl TryIntoCString,
        src_snap: impl Into<SnapKey>,
        dest_pool: &IoCtx<'_>,
        dest_name: impl TryIntoCString,
        dest_options: impl Borrow<ImageOptions>,
    ) -> Result<()> {
        self.rbd_clone_internal(src_image, src_snap, dest_pool, dest_name, dest_options).await
    }

    async fn rbd_clone_internal(
        &self,
        src_image: impl TryIntoCString,
        src_snap: impl Into<SnapKey>,
        dest_pool: &IoCtx<'_>,
        dest_name: impl TryIntoCString,
        dest_options: impl Borrow<ImageOptions>,
    ) -> Result<()> {
        let p_name = src_image.try_into_cstring().map_err(Error::ffi);
        let p_snap = src_snap.into();
        let c_name = dest_name.try_into_cstring().map_err(Error::ffi);
        let c_ioctx = dest_pool.handle.clone();
        let c_opts = dest_options.borrow();

        let span = tracing::debug_span!("IoCtx::rbd_clone_internal", ?p_name, ?p_snap, ?c_ioctx, ?c_name, ?c_opts);

        let p_name = p_name?;
        let c_name = c_name?;
        let sys_opts = c_opts.as_sys_repr()?;

        self.do_op(span, move |p_ioctx| unsafe {
            let err = match p_snap {
                #[cfg(not(feature = "rbd_v1-20"))]
                SnapKey::Id(_) => unreachable!(),
                #[cfg(feature = "rbd_v1-20")]
                SnapKey::Id(id) =>
                    rbd_sys::rbd_clone4(p_ioctx, p_name.as_ptr(), id.0, c_ioctx.as_ptr(), c_name.as_ptr(), sys_opts.as_ptr()),
                SnapKey::Name(name) => rbd_sys::rbd_clone3(
                    p_ioctx,
                    p_name.as_ptr(),
                    name.as_ptr(),
                    c_ioctx.as_ptr(),
                    c_name.as_ptr(),
                    sys_opts.as_ptr(),
                ),
            };
            check_os_error(err).map(|_| ()).map_err(Into::into)
        })
        .await
    }

    /// Remove (delete) an RBD image.
    ///
    /// Fails if converting `name` to a [`CString`] fails.
    pub fn rbd_remove(&self, name: impl TryIntoCString) -> ProgressOp<'_, Result<()>> {
        let name = name.try_into_cstring().map_err(Error::ffi);
        let span = tracing::debug_span!("IoCtx::rbd_remove", ?name);

        self.do_progress_op(span, move |ioctx, cb, data| unsafe {
            check_os_error(rbd_sys::rbd_remove_with_progress(ioctx, name?.as_ptr(), cb, data))?;
            Ok(())
        })
    }

    /// Rename an RBD image.
    ///
    /// Fails if converting `src_name` or `dest_name` to a [`CString`] fails.
    pub async fn rbd_rename(&self, src_name: impl TryIntoCString, dest_name: impl TryIntoCString) -> Result<()> {
        let src_name = src_name.try_into_cstring().map_err(Error::ffi)?;
        let dest_name = dest_name.try_into_cstring().map_err(Error::ffi)?;
        let span = tracing::debug_span!("IoCtx::rbd_rename", ?src_name, ?dest_name);

        self.do_op(span, move |ioctx| unsafe {
            check_os_error(rbd_sys::rbd_rename(ioctx, src_name.as_ptr(), dest_name.as_ptr()))?;
            Ok(())
        })
        .await
    }
}
