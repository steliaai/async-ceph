// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Async Rust bindings for the [librbd](https://docs.ceph.com/en/latest/rbd/api/librbdpy/) C library.
//!
//! The majority of operations are done through the [`Image`] type, which can be obtained from a RADOS
//! [`IoCtx`] through [`Image::open`] or more flexibly through the [`ImageBuilder`]. Functions that do
//! not take an image handle, e.g [`rbd_create`](IoCtx::rbd_clone), are exposed directly on the [`IoCtx`].

use std::{
    ffi::{CString, c_void},
    future::Future,
    mem::ManuallyDrop,
    ptr::{NonNull, null_mut},
};

use futures_util::FutureExt;
use rbd_sys::{rbd_aio_close, rbd_image_t};
use tracing::{Instrument, Span, field};

use crate::{
    Error,
    Result,
    async_rt::{DynExecutor, Executor},
    buf::{BoundedBuf, BoundedBufMut},
    rados::{IoCtx, IoCtxHandle},
    rbd::{
        advisory_locks::AdvisoryLock,
        aio::{aio_submit_ffi, aio_submit_op, create_rbd_completion},
        diff::DiffIterate,
        snap::{SnapInfoList, SnapRemoveFlags},
        watch::{ImageWatchAsync, ImageWatchCb},
    },
    util::{ArcWait, TryIntoCString, UnsafeSendSync, check_os_error},
};

use snap::SnapCreateFlags;

use std::result::Result as StdResult;

pub(crate) mod aio;

pub mod advisory_locks;
pub mod diff;
pub mod features;
mod io_ctx;
pub mod metadata;
pub mod options;
mod progress;
pub mod snap;
pub mod watch;

#[doc(inline)]
pub use progress::*;

pub use options::ImageOptions;

pub use features::{Features, OpFeatures};

pub use metadata::ImageMeta;

use aio::ops;

#[doc(inline)]
pub use aio::ops::ZeroFlags;

#[doc(inline)]
pub use snap::{SnapId, SnapKey, Snapshot, SnapshotById, SnapshotByName};

/// The version of librbd headers used to generate the system bindings that this
/// crate builds on.
///
/// Note that this is unrelated to the Ceph version or the `async-ceph` version.
pub const VERSION: crate::LibVersion = crate::LibVersion {
    major: rbd_sys::LIBRBD_VER_MAJOR,
    minor: rbd_sys::LIBRBD_VER_MINOR,
    patch: rbd_sys::LIBRBD_VER_EXTRA,
};

/// Returns the current version of librbd.
///
/// Note that this is unrelated to the Ceph version or the `async-ceph` version.
pub fn version() -> crate::LibVersion {
    let (mut major, mut minor, mut patch) = (0, 0, 0);
    unsafe { rbd_sys::rbd_version(&mut major, &mut minor, &mut patch) };
    crate::LibVersion {
        major: major as u32,
        minor: minor as u32,
        patch: patch as u32,
    }
}

#[derive(Debug)]
pub(crate) struct ImagePtr {
    ioctx: ManuallyDrop<IoCtxHandle>,
    image: NonNull<c_void>,
}

unsafe impl Send for ImagePtr {}
unsafe impl Sync for ImagePtr {}

impl ImagePtr {
    pub unsafe fn new(ioctx: IoCtxHandle, image: NonNull<c_void>) -> Self {
        Self {
            ioctx: ManuallyDrop::new(ioctx),
            image,
        }
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.image.as_ptr()
    }

    pub fn into_raw_parts(self) -> (IoCtxHandle, NonNull<c_void>) {
        let mut this = ManuallyDrop::new(self);
        let ioctx = unsafe { ManuallyDrop::take(&mut this.ioctx) };
        (ioctx, this.image)
    }

    /// Drop this [`ImagePtr`] synchronously.
    #[tracing::instrument("ImagePtr::blocking_drop", level = "debug")]
    fn blocking_drop(self) -> Result<()> {
        let (_ioctx, image) = self.into_raw_parts();
        check_os_error(unsafe { rbd_sys::rbd_close(image.as_ptr()) })?;
        Ok(())
    }
}

impl Drop for ImagePtr {
    /// Asynchronously drop the [`ImagePtr`].
    ///
    /// This needs to be done as such, since if the `image.close` future is cancelled and
    /// an aio operation is currently in flight, it's possible for this Drop impl to be called
    /// from *within* a librbd completion handler. If this happens, rbd_close internally
    /// registers its own completion and waits for it... but it's queued after the current one
    /// and we have a deadlock.
    #[tracing::instrument("ImagePtr::drop", level = "debug")]
    fn drop(&mut self) {
        unsafe extern "C" fn drop_ioctx(comp: *mut c_void, ioctx: *mut c_void) {
            if let Err(e) = check_os_error(unsafe { rbd_sys::rbd_aio_get_return_value(comp) }) {
                tracing::error!("rbd_aio_close completion failed: {e}");
            }
            // SAFETY: ioctx_ptr is passed to created_rbd_completion below and `ioctx` is
            // forgotten before the aio operation is submitted, so we can take ownership of it
            unsafe {
                rbd_sys::rbd_aio_release(comp);
                drop(IoCtxHandle::from_raw(ioctx.cast()));
            }
        }

        let ioctx = unsafe { ManuallyDrop::take(&mut self.ioctx) };
        let ioctx_ptr = ArcWait::as_raw(&ioctx);
        let comp = match create_rbd_completion(Some(drop_ioctx), ioctx_ptr as *mut c_void) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("create_rbd_completion failed during ImagePtr drop: {e}");
                return;
            }
        };

        // SAFETY: we leak the ioctx_handle, so no double free occurs once rbd_aio_close completes
        std::mem::forget(ioctx);
        if let Err(e) = check_os_error(unsafe { rbd_sys::rbd_aio_close(self.image.as_ptr(), comp) }) {
            // SAFETY: the aio operation wasn't submitted, so we have to reclaim the ioctx_ptr
            drop(unsafe { ArcWait::from_raw(ioctx_ptr) });
            tracing::error!("rbd_aio_close failed: {e}");
        }
    }
}

pub(crate) type ImageHandle = ArcWait<ImagePtr>;

#[doc(hidden)]
#[derive(Debug)]
pub enum WithId {
    Name(CString),
    Id(CString),
}

/// Builder struct to configure the way an RBD image will be opened.
///
/// For the common use-case of opening the current snapshot of an image by name
/// in read-write mode, consider using [`Image::open`] instead.
///
/// # Examples
///
/// Opening an image's writable head by name, in read-only mode (this has some quirks,
/// see [`ImageBuilder::read_only`] for more details):
///
/// ```no_run
/// # use async_ceph::{rados::*, rbd::*};
/// # async fn test(cluster: RadosClient<'_>) -> Result<(), Box<dyn std::error::Error>> {
/// let pool = cluster.create_io_ctx("rbd").await?;
///
/// let image = ImageBuilder::new(&pool)
///     .read_only(true)
///     .name("my-image")?
///     .open()
///     .await?;
///
/// # Ok(())
/// # }
/// ```
///
/// Opening an image's writable head by its unique ID:
///
/// ```no_run
/// # use async_ceph::{rados::*, rbd::*};
/// # async fn test(cluster: RadosClient<'_>) -> Result<(), Box<dyn std::error::Error>> {
/// let pool = cluster.create_io_ctx("rbd").await?;
///
/// let image = ImageBuilder::new(&pool)
///     .id("134a74b0dc51")?
///     .open()
///     .await?;
///
/// # Ok(())
/// # }
/// ```
///
/// Opening an image's specific snapshot by name (always read only):
///
/// ```no_run
/// # use async_ceph::{rados::*, rbd::*};
/// # async fn test(cluster: RadosClient<'_>) -> Result<(), Box<dyn std::error::Error>> {
/// let pool = cluster.create_io_ctx("rbd").await?;
///
/// let image = ImageBuilder::new(&pool)
///     .name("my-image")?
///     .snap_name("snapshot1")?
///     .open()
///     .await?;
///
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct ImageBuilder<'a, Id = ()> {
    executor: &'a dyn DynExecutor,
    ioctx: IoCtxHandle,
    id: Id,
    snap_name: Option<CString>,
    read_only: bool,
}

impl<'a> ImageBuilder<'a, ()> {
    /// Create a new [`ImageBuilder`] to open an image in the pool associated with the given [`IoCtx`].
    pub fn new(ioctx: &IoCtx<'a>) -> Self {
        Self {
            executor: ioctx.executor,
            ioctx: ioctx.handle.clone(),
            id: (),
            snap_name: None,
            read_only: false,
        }
    }
}

impl<'a, T> ImageBuilder<'a, T> {
    /// Specify the image snapshot to be opened.
    ///
    /// This is optional. By default, the toplevel snapshot will be opened.
    ///
    /// Fails if converting `name` to a [`CString`] fails.
    pub fn snap_name<N: TryIntoCString>(self, name: N) -> StdResult<Self, N::Error> {
        Ok(Self {
            snap_name: Some(name.try_into_cstring()?),
            ..self
        })
    }

    /// If set to true, the image will be opened in read-only mode.
    ///
    /// This is mainly intended for clients that lack write permissions. Since a watch object
    /// is considered a write, no watch will be established by librbd and as such image metadata
    /// may become stale. Hence opening images as read-only should be avoided for long-running
    /// operations.
    pub fn read_only(self, read_only: bool) -> Self {
        Self { read_only, ..self }
    }
}

impl<'a> ImageBuilder<'a, ()> {
    /// Specify the image to open by its ID.
    ///
    /// Fails if converting `id` to a [`CString`] fails.
    pub fn id<T: TryIntoCString>(self, id: T) -> StdResult<ImageBuilder<'a, WithId>, T::Error> {
        Ok(ImageBuilder {
            executor: self.executor,
            ioctx: self.ioctx,
            id: WithId::Id(id.try_into_cstring()?),
            snap_name: self.snap_name,
            read_only: self.read_only,
        })
    }

    /// Specify the image to open by its name.
    ///
    /// Fails if converting `name` to a [`CString`] fails.
    pub fn name<T: TryIntoCString>(self, name: T) -> StdResult<ImageBuilder<'a, WithId>, T::Error> {
        Ok(ImageBuilder {
            executor: self.executor,
            ioctx: self.ioctx,
            id: WithId::Name(name.try_into_cstring()?),
            snap_name: self.snap_name,
            read_only: self.read_only,
        })
    }
}

impl<'a> ImageBuilder<'a, WithId> {
    /// Asynchronously open an RBD image according to this [`ImageBuilder`]'s configuration.
    #[tracing::instrument("ImageBuilder::open", level = "debug")]
    pub async fn open(self) -> Result<Image<'a>> {
        let open_fn = match (&self.id, self.read_only) {
            (WithId::Name(_), false) => rbd_sys::rbd_aio_open,
            (WithId::Id(_), false) => rbd_sys::rbd_aio_open_by_id,
            (WithId::Name(_), true) => rbd_sys::rbd_aio_open_read_only,
            (WithId::Id(_), true) => rbd_sys::rbd_aio_open_by_id_read_only,
        };

        let executor = self.executor;
        let snap_name = self.snap_name;
        let identifier = match self.id {
            WithId::Name(n) => n,
            WithId::Id(id) => id,
        };

        struct OpData {
            ioctx: Option<IoCtxHandle>,
            image: UnsafeSendSync<rbd_image_t>,
        }

        impl Drop for OpData {
            fn drop(&mut self) {
                // If `ImageBuilder::open` is forgotten/cancelled, we may have to free the rbd_image_it
                // (provided the call succeeded)
                if let Some(image) = NonNull::new(*self.image) {
                    drop(unsafe { ImagePtr::new(self.ioctx.take().unwrap(), image) });
                }
            }
        }

        let data = OpData {
            ioctx: Some(self.ioctx),
            image: UnsafeSendSync::new(null_mut()),
        };

        let (mut op_data, res) = unsafe {
            aio_submit_ffi(data, move |data, comp| {
                let id_ptr = identifier.as_ptr();
                let snap_ptr = snap_name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
                open_fn(data.ioctx.as_ref().unwrap().as_ptr(), id_ptr, &mut *data.image, snap_ptr, comp)
            })
        }
        .await;
        res?;

        let image = NonNull::new(std::mem::take(&mut op_data.image)).ok_or_else(|| {
            crate::Error::unexpected("Got null rbd_image_t despite open succeeding. This is likely a librbd bug.")
        })?;

        let image_ptr = unsafe { ImagePtr::new(op_data.ioctx.take().unwrap(), image) };
        Ok(Image {
            manually_drop_handle: ManuallyDrop::new(ArcWait::new(image_ptr)),
            executor,
        })
    }
}

/// Safe asynchronous wrapper around a librbd `rbd_image_t`.
///
/// To open an image, use [`Image::open`].
///
/// # Closing
///
/// The [`Drop`] implementation for this type blocks until the image is no longer used.
/// In async code, prefer using [`Image::close`] instead.
///
/// <div class="warning">
///
/// Because of this, holding an owned [`LockGuard`] or [`SharedLockGuard`] past their associated
/// `Image` in the same scope it is dropped in **will lead to a deadlock**, as the image will wait
/// until it's no longer used. Make sure unlock or drop them before closing the image.
///
/// </div>
///
/// [`LockGuard`]: advisory_locks::LockGuard
/// [`SharedLockGuard`]: advisory_locks::SharedLockGuard
#[derive(Debug)]
pub struct Image<'a> {
    manually_drop_handle: ManuallyDrop<ImageHandle>,
    executor: &'a dyn DynExecutor,
}

impl<'a> Drop for Image<'a> {
    #[tracing::instrument("Image::drop", level = "debug")]
    fn drop(&mut self) {
        let handle = unsafe { ManuallyDrop::take(&mut self.manually_drop_handle) };
        if let Err(e) = ArcWait::blocking_wait_unwrap(handle).blocking_drop() {
            tracing::error!("rbd_close failed: {e}");
        }
    }
}

impl<'a> Image<'a> {
    pub(crate) fn handle(&self) -> &ImageHandle {
        &self.manually_drop_handle
    }

    /// Perform a blocking operation on the async executor's threadpool.
    ///
    /// # Safety Notes
    /// The returned `rbd_image_t` is valid for the duration of the call.
    ///
    /// The operation must be safe to perform concurrently on the image. If it is not, the caller must
    /// guarantee that `&self` is a unique reference.
    pub(crate) fn do_blocking<R, F>(&self, blocking: F) -> impl Future<Output = R> + Send + 'static
    where
        R: Send + 'static,
        F: FnOnce(ImageHandle) -> R + Send + 'static, {
        let image = self.handle().clone();
        self.executor
            .spawn_blocking(move || blocking(image))
            .map(|join_result| join_result.expect("failed to join async task"))
    }

    /// Create a [`ProgressOp`] wrapping a progress-reporting operation on this image.
    ///
    /// # Safety Notes
    /// The returned `rbd_image_t` is valid for the duration of the call.
    ///
    /// The operation must be safe to perform concurrently on the image. If it is not, the caller must
    /// guarantee that `&self` is a unique reference.
    pub(crate) fn do_progress_op<R, F>(&self, op: F) -> ProgressOp<'_, R>
    where
        R: Send + 'static,
        F: FnOnce(rbd_image_t, rbd_sys::librbd_progress_fn_t, *mut c_void) -> R + Send + 'static, {
        let image = self.handle().clone();
        ProgressOp::new(self.executor, move |cb, data| op(image.as_ptr(), cb, data))
    }

    /// Asynchronously read `buf.bytes_total()` bytes at `offset` from the start of this [`Image`].
    ///
    /// Because this operation temporarily gives ownership of the buffer to the RBD asynchronous operation,
    /// it must be passed by value and can be reclaimed by awaiting the result.
    ///
    /// To read less than the full capacity of the buffer, consider using [`BoundedBuf::slice`] to write
    /// to an owned view of the buffer instead.
    ///
    /// For serial reads in a loop, use the following pattern to reclaim the buffer after every iteration:
    ///
    /// ```no_run
    /// # async fn test() -> async_ceph::Result<()> {
    /// # let mut image: async_ceph::rbd::Image = panic!();
    /// # let count = 10;
    /// let mut buf = Vec::with_capacity(64);
    /// let mut res;
    /// for _ in 0 .. count {
    ///     (buf, res) = image.read(0, buf).await;
    ///     let n_read = res?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument("Image::read", level = "debug", skip(buf))]
    pub fn read<B>(&self, offset: u64, buf: B) -> impl Future<Output = (B, Result<usize>)> + Send + use<B>
    where
        B: BoundedBufMut + Send, {
        aio_submit_op(self, ops::Read::new(buf, offset, 0))
    }

    /// Asynchronously read as many bytes as possible at `offset` in this [`Image`] into a collection of
    /// contiguous buffers.
    ///
    /// Because this operation temporarily gives ownership of the buffers to the RBD asynchronous operation,
    /// they must passed by value and can be reclaimed by awaiting the result.
    ///
    /// This method can write into the uninitialized portion of the buffers, so consider bounding them with
    /// [`BoundedBuf::slice`] if you wish to precisely control how many bytes should be read.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn test(offset: u64) -> async_ceph::Result<()> {
    /// # let image: async_ceph::rbd::Image = panic!();
    /// // create 10 buffers of at least 64 bytes each
    /// let bufs: Vec<_> = (0..10).map(|_| Vec::with_capacity(64)).collect();
    ///
    /// let (bufs, res) = image.read_vectored(offset, bufs).await;
    /// let n_read = res?;
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument("Image::read_vectored", level = "debug", skip(self, bufs))]
    pub fn read_vectored<B>(&self, offset: u64, bufs: Vec<B>) -> impl Future<Output = (Vec<B>, Result<usize>)> + Send + use<B>
    where
        B: BoundedBufMut + Send, {
        let op_result = aio::ops::ReadVectored::new(bufs, offset).map(|op| aio_submit_op(self, op));
        async move {
            match op_result {
                Ok(op) => op.await,
                Err((bufs, e)) => (bufs, Err(e)),
            }
        }
    }

    /// Asynchronously write `buf.bytes_init()` bytes at `offset` from the start of this [`Image`].
    ///
    /// Because this operation temporarily gives ownership of the buffer to the RBD asynchronous operation,
    /// it must be passed by value and can be reclaimed by awaiting the result.
    ///
    /// To write less than the full buffer, consider using [`BoundedBuf::slice`] to read from
    /// an owned view of the buffer instead.
    ///
    /// For serial writes in a loop, use the following pattern to reclaim the buffer after every iteration:
    ///
    /// ```no_run
    /// # async fn test() -> async_ceph::Result<()> {
    /// # let mut image: async_ceph::rbd::Image = panic!();
    /// # let count = 10;
    /// let mut buf = vec![0u8; 64];
    /// let mut res;
    /// for _ in 0 .. count {
    ///     (buf, res) = image.read(0, buf).await;
    ///     res?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument("Image::write", level = "debug", skip(buf))]
    pub fn write<B>(&self, offset: u64, buf: B) -> impl Future<Output = (B, Result<()>)> + Send + use<B>
    where
        B: BoundedBuf + Send, {
        aio_submit_op(self, ops::Write::new(buf, offset, 0))
    }

    /// Asynchronously write data stored in a collection of contiguous buffers to the specified offset in this
    /// [`Image`].
    ///
    /// Because this operation temporarily gives ownership of the buffers to the RBD asynchronous operation,
    /// they must passed by value and can be reclaimed by awaiting the result.
    ///
    /// To write less than the full buffers, consider using [`BoundedBuf::slice`] to read from
    /// an owned view instead.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn test(offset: u64) -> async_ceph::Result<()> {
    /// # let image: async_ceph::rbd::Image = panic!();
    /// // create buffers with some data
    /// let bufs = vec!["foo", "bar", "baz"];
    ///
    /// // writes "foobarbaz"
    /// let (bufs, res) = image.write_vectored(offset, bufs).await;
    /// res?;
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument("Image::write_vectored", level = "debug", skip(self, bufs))]
    pub fn write_vectored<B>(&self, offset: u64, bufs: Vec<B>) -> impl Future<Output = (Vec<B>, Result<()>)> + Send + use<B>
    where
        B: BoundedBuf + Send, {
        let op_result = aio::ops::WriteVectored::new(bufs, offset).map(|op| aio_submit_op(self, op));
        async move {
            match op_result {
                Ok(op) => op.await,
                Err((bufs, e)) => (bufs, Err(e)),
            }
        }
    }

    /// Asynchronously mark the provided range's underlying storage as unused so that it can be reclaimed
    /// by the block device.
    ///
    /// This is typically used when a filesystem is stored in the writable image. Since Ceph does not know
    /// about the filesystem, removed files still take space on the block device. This method can be used
    /// by the filesystem to send discard flush commands to the block device to free up blocks.
    #[tracing::instrument("Image::discard", level = "debug")]
    pub fn discard(&self, offset: u64, len: u64) -> impl Future<Output = Result<()>> + Send + 'static {
        aio_submit_op(self, ops::Discard::new(offset, len)).map(|r| r.1)
    }

    /// Write zeroes to the provide range of the image.
    ///
    /// `zflags` determines how the zeroes are to be written. By default this is a "sparse" operation:
    /// Any full zero extents in the range are simply marked as being zero. To actually allocate storage with
    /// "physical" zeroes, pass [`ZeroFlags::THICK_PROVISION`].
    #[tracing::instrument("Image::write_zeroes", level = "debug")]
    pub fn write_zeroes(&self, offset: u64, len: usize, zflags: ZeroFlags) -> impl Future<Output = Result<()>> + Send + 'static {
        aio_submit_op(self, ops::WriteZeroes::new(offset, len, zflags, 0)).map(|r| r.1)
    }

    /// Asynchronously write `buf.bytes_init()` bytes repeated `repeat` times at `offset`.
    ///
    /// Because this operation temporarily gives ownership of the buffer to the RBD asynchronous operation,
    /// it must be passed by value and can be reclaimed by awaiting the result.
    ///
    /// To write less than the full buffer, consider using [`BoundedBuf::slice`] to read from
    /// an owned view of the buffer instead.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn test() -> async_ceph::Result<()> {
    /// # let mut image: async_ceph::rbd::Image = panic!();
    /// // write "foo" repeated 5 times
    /// image.write_same(0, 5, "foo").await.1?;
    ///
    /// let (buf, n_read) = image.read(0, Vec::with_capacity(15)).await;
    /// assert_eq!(&buf[.. n_read?], b"foofoofoofoofoo");
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument("Image::write_same", level = "debug", skip(self, buf))]
    pub fn write_same<B>(&self, offset: u64, repeat: usize, buf: B) -> impl Future<Output = (B, Result<()>)> + Send + use<B>
    where
        B: BoundedBuf + Send, {
        let op_result = ops::WriteSame::new(buf, offset, repeat, 0).map(|op| aio_submit_op(self, op));
        async move {
            match op_result {
                Ok(op) => op.await,
                Err((data, e)) => (data, Err(e)),
            }
        }
    }

    /// Atomically write `buf.bytes_init()` bytes at `offset` from the start of this [`Image`] *if
    /// the bytes in `cmp` match the image contents at the same position.*
    ///
    /// If the operation itself is successful, returns a [`Result<(), u64>`](std::result::Result) indicating
    /// a successful write with [`Ok`] and a mismatch with an [`Err`] containing the index of the first
    /// mismatched byte in `cmp`.
    ///
    /// Because this operation temporarily gives ownership of the buffers to the RBD asynchronous operation,
    /// they must be passed by value and can be reclaimed by awaiting the result.
    ///
    /// To write less than the full buffer, consider using [`BoundedBuf::slice`] to read from
    /// an owned view of the buffer instead.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn test() -> async_ceph::Result<()> {
    /// # let mut image: async_ceph::rbd::Image = panic!();
    /// // write "abc123" to the image at offset 0
    /// image.write(0, "abc123").await.1?;
    ///
    /// // compare with "abcdef" and write "foobar" on match
    /// // "abc123" != "abcdef", so this fails and returns the mismatch index (3)
    /// let result = image.compare_and_write(0, "abcdef", "foobar").await.1?;
    /// assert_eq!(result, Err(3));
    ///
    /// // compare with "abc123" and write "foobar" on match (this succeeds)
    /// let result = image.compare_and_write(0, "abc123", "foobar").await.1?;
    /// assert_eq!(result, Ok(()));
    ///
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument("Image::compare_and_write", level = "debug", skip(self, buf, cmp))]
    pub fn compare_and_write<C, B>(
        &self,
        offset: u64,
        cmp: C,
        buf: B,
    ) -> impl Future<Output = ((C, B), Result<StdResult<(), u64>>)> + Send + use<B, C>
    where
        C: BoundedBuf + Send,
        B: BoundedBuf + Send, {
        let op_result = aio::ops::CompareAndWrite::new(cmp, buf, offset, 0).map(|op| aio_submit_op(self, op));
        async move {
            match op_result {
                Ok(op) => op.await,
                Err((data, e)) => (data, Err(e)),
            }
        }
    }

    /// Atomically write the bytes in `bufs` at `offset` from the start of this [`Image`] *if
    /// the bytes in `cmps` match the image contents at the same position.*
    ///
    /// For more information see [`compare_and_write`](Image::compare_and_write).
    ///
    /// Because this operation temporarily gives ownership of the buffers to the RBD asynchronous operation,
    /// they must be passed by value and can be reclaimed by awaiting the result.
    ///
    /// To write less than the full buffers, consider using [`BoundedBuf::slice`] to read from
    /// an owned view of the buffers instead.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn test() -> async_ceph::Result<()> {
    /// # let mut image: async_ceph::rbd::Image = panic!();
    /// // compare with "abcdef" and write "foobar" on match1
    /// let cmps = vec!["a", "bcd", "ef"];
    /// let writes = vec!["foo", "bar"];
    /// let ((cmps, writes), io_res) = image.compare_and_write_vectored(0, cmps, writes).await;
    /// let cmp_result = io_res?;
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "rbd_compare_and_write_iovec")]
    // tracing::instrument macro breaks doc auto-cfg, we have to do it manually
    #[cfg_attr(docsrs, doc(cfg(feature = "rbd_compare_and_write_iovec")))]
    #[tracing::instrument("Image::compare_and_write_vectored", level = "debug", skip(self, bufs, cmps))]
    #[allow(clippy::type_complexity)]
    pub fn compare_and_write_vectored<C, B>(
        &self,
        offset: u64,
        cmps: Vec<C>,
        bufs: Vec<B>,
    ) -> impl Future<Output = ((Vec<C>, Vec<B>), Result<StdResult<(), u64>>)> + Send + use<B, C>
    where
        C: BoundedBuf + Send,
        B: BoundedBuf + Send, {
        let op_result = aio::ops::CompareAndWriteVectored::new(cmps, bufs, offset, 0).map(|op| aio_submit_op(self, op));
        async move {
            match op_result {
                Ok(op) => op.await,
                Err((data, e)) => (data, Err(e)),
            }
        }
    }

    /// Create a builder struct for iterating over the difference between two versions of this image.
    ///
    /// By default, the entire image range is considered and source version is interpreted as the
    /// image state when it was created or cloned from its parent snapshot, if it has one.
    ///
    /// Note that the latter implies that extents created in the this parent will **not** be returned.
    /// Use [`DiffIterate::include_parent`] to include them.
    ///
    /// See the [`DiffIterate`] documentation for more information.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use async_ceph::rbd::SnapId;
    /// # async fn test() -> std::io::Result<()> {
    /// # let image: async_ceph::rbd::Image = panic!();
    /// let extents = image
    ///     .diff_iterate()
    ///     .from_snap(c"foo")        // from snapshot "foo"
    ///     .offset(1024u64.pow(3))   // at offset 1GiB
    ///     .len(2 * 1024u64.pow(3))  // over a 2GiB region
    ///     .collect()
    ///     .await?;
    ///
    /// for e in extents {
    ///     println!("offset: {}, len: {}, exists: {}", e.offset, e.len, e.exists);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn diff_iterate(&self) -> DiffIterate<'_> {
        DiffIterate::new(self)
    }

    /// Asynchronously open an RBD image by name.
    ///
    /// To open an RBD image by ID or in read-only mode, use the [`ImageBuilder`] directly.
    pub async fn open(ctx: &IoCtx<'a>, name: impl TryIntoCString) -> Result<Self> {
        ImageBuilder::new(ctx).name(name).map_err(Error::ffi)?.open().await
    }

    /// Waits until all operations on this [`Image`] are complete, then closes it asynchronously.
    ///
    /// # Cancellation
    ///
    /// If this future is dropped or cancelled the image will close in the background after all in-flight
    /// operations have completed. [`RadosClient::shutdown`] can wait for such a cancelled close.
    ///
    /// # Warning
    ///
    /// Because of this, holding an owned [`LockGuard`] or [`SharedLockGuard`] past their associated
    /// `Image` in the same scope it is dropped in **will lead to a deadlock**, as the image will wait
    /// until it's no longer used. Make sure unlock or drop them before closing the image.
    ///
    /// [`RadosClient::shutdown`]: crate::rados::RadosClient::shutdown
    /// [`LockGuard`]: advisory_locks::LockGuard
    /// [`SharedLockGuard`]: advisory_locks::SharedLockGuard
    pub fn close(self) -> impl Future<Output = Result<()>> + Send {
        let span = tracing::debug_span!("Image::close", ?self);

        // Since Image implements Drop, we have to manually extract the handle without
        // running the destructor
        let mut this = ManuallyDrop::new(self);
        let handle = unsafe { ManuallyDrop::take(&mut this.manually_drop_handle) };

        async move {
            let (ioctx, image) = ArcWait::wait_unwrap(handle).await.into_raw_parts();

            // keep ioctx alive for the duration of the op
            let (_, res) = unsafe { aio_submit_ffi(ioctx, move |_, c| rbd_aio_close(image.as_ptr(), c)) }.await;
            res.map(|_| ())
        }
        .instrument(span)
    }

    /// Wait until IO operations on this image have been flushed to disk.
    #[tracing::instrument("Image::flush", level = "debug")]
    pub async fn flush(&self) -> Result<()> {
        let h = self.handle().clone();
        let (_, res) = unsafe { aio_submit_ffi(h, |h, c| rbd_sys::rbd_aio_flush(h.as_ptr(), c)) }.await;
        res.map(|_| ())
    }

    /// Instruct librbd to drop all cached data for this image.
    ///
    /// Note that this means that the next call to [`Image::get_metadata`] and the like
    /// will have to request the data from the cluster.
    #[tracing::instrument("Image::invalidate_cache", level = "debug")]
    pub fn invalidate_cache(&self) -> Result<()> {
        check_os_error(unsafe { rbd_sys::rbd_invalidate_cache(self.handle().as_ptr()) })?;
        Ok(())
    }

    /// Resize this [`Image`] to `size`.
    ///
    /// Note that the image will not shrink below its current size unless `allow_shrink` is true.
    /// Instead, the method will return an error with [`ErrorKind::InvalidInput`](std::io::ErrorKind::InvalidInput).
    ///
    /// This returns a [`ProgressOp`] future, so the [`ProgressOp::with_progress`] combinator can be
    /// used to monitor this operation's progress.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn test() -> async_ceph::Result<()> {
    /// # let mut image: async_ceph::rbd::Image = panic!();
    /// const MIB: u64 = 1024 * 1024;
    ///
    /// // increase the size of the image to 512 MiB
    /// image.resize(512 * MIB, false)
    ///     // provide a progress callback
    ///     .with_progress_cb(|p| println!("resizing: {}%", p.as_percentage()))
    ///     .await?;
    ///
    /// // decrease it to 256 MiB (fails, since allow_shrink is false)
    /// let err = image.resize(256 * MIB, false).await.unwrap_err();
    /// assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    ///
    /// // succeeds, since allow_shrink is true
    /// image.resize(256 * MIB, true).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn resize(&self, size: u64, allow_shrink: bool) -> ProgressOp<'_, Result<()>> {
        let span = tracing::debug_span!("Image::resize", size, allow_shrink);
        self.do_progress_op(move |image, cb, data| unsafe {
            let _enter = span.enter();
            check_os_error(rbd_sys::rbd_resize2(image, size, allow_shrink, cb, data))?;
            Ok(())
        })
    }

    /// Fetch information about the image.
    ///
    /// Most often this is a non-blocking operation, but librbd may need to request updated
    /// information from the cluster, hence it being `async`.
    #[tracing::instrument("Image::get_metadata", fields(data_type = std::any::type_name::<D>()), err)]
    pub async fn get_metadata<D: ImageMeta>(&self) -> Result<D> {
        self.do_blocking(move |image| unsafe { D::get(image.as_ptr()) }).await
    }

    /// Returns basic image information.
    pub async fn stat(&self) -> Result<metadata::ImageStat> {
        self.get_metadata().await
    }

    /// Returns the features enabled on this image.
    pub async fn features(&self) -> Result<Features> {
        self.get_metadata().await
    }

    /// Returns the operation features enabled on this image.
    pub async fn op_features(&self) -> Result<OpFeatures> {
        self.get_metadata().await
    }

    /// Returns the latest access/creation/modification timestamps for this image.
    pub async fn timestamps(&self) -> Result<metadata::ImageTimestamps> {
        self.get_metadata().await
    }

    /// Returns information about the group this image is in.
    pub async fn group(&self) -> Result<Option<metadata::ImageGroup>> {
        self.get_metadata().await
    }

    /// Returns information about the parent of this image.
    pub async fn parent(&self) -> Result<Option<metadata::ImageParent>> {
        self.get_metadata().await
    }

    /// Returns information about the images this one is the parent of.
    pub async fn children(&self) -> Result<Vec<metadata::LinkedImageSpec>> {
        self.get_metadata::<metadata::ImageChildren>().await.map(|c| c.0)
    }

    /// Returns information about all (transitive) descendants of this image.
    pub async fn descendants(&self) -> Result<Vec<metadata::LinkedImageSpec>> {
        self.get_metadata::<metadata::ImageDescendants>().await.map(|d| d.0)
    }

    /// Returns information about the watchers currently registered to this image.
    pub async fn watchers(&self) -> Result<Vec<metadata::ImageWatcher>> {
        self.get_metadata::<metadata::ImageWatchers>().await.map(|w| w.0)
    }

    /// Returns extensive information about the image.
    pub async fn full_info(&self) -> Result<metadata::ImageFullInfo> {
        self.get_metadata().await
    }

    /// Returns basic information (id, name and size) about each snapshot of this image.
    pub async fn snap_list(&self) -> Result<SnapInfoList> {
        self.get_metadata().await
    }

    /// Get the snapshot limit for this image. Attempting to create more snapshots than this will
    /// result in an error.
    ///
    /// Default to [`u64::MAX`].
    pub async fn snap_limit(&self) -> Result<u64> {
        self.get_metadata::<snap::SnapLimit>().await.map(|limit| limit.0)
    }

    /// Set the snapshot limit for this image. Attempting to create more snapshots than this will
    /// result in an error.
    ///
    /// By default, this is unlimited ([`u64::MAX`]).
    ///
    /// # Example
    ///
    /// Preventing snapshot creation on an image:
    ///
    /// ```no_run
    /// # async fn test() -> async_ceph::Result<()> {
    /// # let mut image: async_ceph::rbd::Image = panic!();
    /// image.snap_limit_set(0).await?;
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument("Image::snap_limit_set", level = "debug", err)]
    pub async fn snap_limit_set(&self, limit: u64) -> Result<()> {
        self.do_blocking(move |image| unsafe {
            check_os_error(rbd_sys::rbd_snap_set_limit(image.as_ptr(), limit))?;
            Ok(())
        })
        .await
    }

    /// Get a [`Snapshot`] object that refers to one of this image's snapshots by name.
    pub fn snap_by_name<N: TryIntoCString>(&self, name: N) -> StdResult<SnapshotByName<'_>, N::Error> {
        Snapshot::from_name(self, name)
    }

    /// Get a [`Snapshot`] object that refers to one of this image's snapshots by ID.
    pub fn snap_by_id(&self, id: SnapId) -> SnapshotById<'_> {
        Snapshot::from_id(self, id)
    }

    /// Get a [`Snapshot`] object that refers to one of this image's snapshots by either a name or an ID.
    pub fn snap_by_key(&self, key: SnapKey) -> Snapshot<'_> {
        Snapshot::from_key(self, key)
    }

    /// Check whether a snapshot identified by this [`SnapKey`] (either a name or unique ID) exists.
    pub async fn snap_exists(&self, key: SnapKey) -> Result<bool> {
        self.snap_by_key(key).exists().await
    }

    /// Create a read-only snapshot of the image.
    ///
    /// This will normally quiesce the image (block all write IO) during creation to ensure that all clients
    /// can rely on the snapshot being a consistent save point. To skip this at the cost of a possibly
    /// inconsistent snapshot, specify [`SnapCreateFlags::SKIP_QUIESCE`].
    ///
    /// Similarly, you can instruct RBD to ignore any quiesce errors and create the snapshot anyway using
    /// [`SnapCreateFlags::IGNORE_QUIESCE_ERROR`].
    ///
    /// # Example
    ///
    /// Create a snapshot named `foo`:
    ///
    /// ```no_run
    /// # async fn test() -> async_ceph::Result<()> {
    /// # let mut image: async_ceph::rbd::Image = panic!();
    /// image.snap_create("foo", Default::default()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn snap_create(&self, name: impl TryIntoCString, flags: SnapCreateFlags) -> ProgressOp<'_, Result<()>> {
        let span = tracing::debug_span!("Image::snap_create", name = field::Empty, ?flags);
        let name_result = name.try_into_cstring().map_err(Error::ffi);

        self.do_progress_op(move |image, cb, data| {
            let _guard = span.enter();
            let name = name_result?;
            span.record("name", field::debug(&name));

            check_os_error(unsafe { rbd_sys::rbd_snap_create2(image, name.as_ptr(), flags.bits(), cb, data) })
                .inspect_err(|err| tracing::error!("rbd_snap_create2 failed: {err}"))?;
            Ok(())
        })
    }

    /// Remove a read-only snapshot of the image by its name.
    ///
    /// See the [`Snapshot::remove`] documentation for more information.
    pub fn snap_remove(&self, name: impl TryIntoCString, flags: SnapRemoveFlags) -> ProgressOp<'_, Result<()>> {
        let span = tracing::debug_span!("Image::snap_remove", name = field::Empty, ?flags);
        let name_result = name.try_into_cstring().map_err(Error::ffi);

        self.do_progress_op(move |image, cb, data| {
            let _guard = span.enter();
            let name = name_result?;
            span.record("name", field::debug(&name));

            check_os_error(unsafe { rbd_sys::rbd_snap_remove2(image, name.as_ptr(), flags.bits(), cb, data) })
                .inspect_err(|err| tracing::error!("rbd_snap_remove2 failed: {err}"))?;
            Ok(())
        })
    }

    /// Remove a read-only snapshot of the image by its ID.
    ///
    /// Due to limitations of the librbd API, this is less flexible than [`Image::snap_remove`]. Namely its
    /// progress cannot be tracked and [`SnapRemoveFlags`] cannot be specified.
    #[tracing::instrument("Image::snap_remove_by_id", level = "debug", err)]
    pub async fn snap_remove_by_id(&self, id: SnapId) -> Result<()> {
        self.do_blocking(move |image| unsafe {
            check_os_error(rbd_sys::rbd_snap_remove_by_id(image.as_ptr(), id.0))?;
            Ok(())
        })
        .await
    }

    /// Rollback the live image to the snapshot with the given name.
    ///
    /// This resets the image's writable head to the state it was in when the snapshot was created,
    /// so must be used carefully to avoid data loss.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn test() -> async_ceph::Result<()> {
    /// # let mut image: async_ceph::rbd::Image = panic!();
    /// // rollback with a progress callback
    /// image.snap_rollback("my-snapshot")
    ///     .with_progress_cb(|p| println!("rolling back: {}%", p.as_percentage()))
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn snap_rollback(&self, name: impl TryIntoCString) -> ProgressOp<'_, Result<()>> {
        let span = tracing::debug_span!("Image::snap_rollback", name = field::Empty);
        let name_result = name.try_into_cstring().map_err(Error::ffi);
        let image = self.handle().clone();

        ProgressOp::new(self.executor, move |cb, data| {
            let _guard = span.enter();
            let name = name_result?;
            span.record("name", field::debug(&name));

            check_os_error(unsafe { rbd_sys::rbd_snap_rollback_with_progress(image.as_ptr(), name.as_ptr(), cb, data) })
                .inspect_err(|err| tracing::error!("rbd_snap_rollback_with_progress failed: {err}"))?;
            Ok(())
        })
    }

    /// Point this [`Image`] object to the given snapshot for reads. Unless [`SnapKey::HEAD`] is passed,
    /// the image becomes read-only.
    ///
    /// `snap` accepts a multitude of argument types that can identify a snapshot, namely:
    /// - [`SnapKey`];
    /// - A snapshot integer ID ([`SnapId`]);
    /// - A snapshot name as a C-string;
    /// - A [`Snapshot`] value or reference.
    ///
    /// To "undo" this method, call it with [`SnapKey::HEAD`].
    #[tracing::instrument("Image::snap_set", level = "debug", fields(snap = field::Empty), err)]
    pub async fn snap_set(&self, snap: impl Into<SnapKey>) -> Result<()> {
        let snap = snap.into();
        Span::current().record("snap", field::debug(&snap));

        self.do_blocking(move |image| unsafe {
            check_os_error(match snap {
                SnapKey::Id(id) => rbd_sys::rbd_snap_set_by_id(image.as_ptr(), id.0),
                SnapKey::Name(name) => rbd_sys::rbd_snap_set(image.as_ptr(), name.as_ptr()),
            })?;
            Ok(())
        })
        .await
    }

    /// Access advisory locking APIs on this [`Image`].
    pub fn advisory_lock(&self) -> AdvisoryLock<'a, '_> {
        AdvisoryLock::new(self)
    }

    /// Returns an [`ImageWatchAsync`] stream that can be used to be asynchronously notified of
    /// updates to this image's metadata.
    ///
    /// This is a convenience method for [`ImageWatchAsync::new`].
    pub fn watch(&self) -> Result<ImageWatchAsync<'_>> {
        ImageWatchAsync::new(self)
    }

    /// Create an image watch that invokes the specified (blocking) callback whenever this image's
    /// metadata changes.
    ///
    /// Once the [`ImageWatchCb`] is dropped, the callback stops being invoked.
    ///
    /// This is a convenience method for [`ImageWatchCb::new`].
    pub fn watch_cb<F>(&self, callback: F) -> Result<ImageWatchCb<'_>>
    where
        F: FnMut() + Send + 'static, {
        ImageWatchCb::new(self, callback)
    }
}
