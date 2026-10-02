// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Safe abstractions around the update watcher librbd APIs.
//!
//! This modules provides two ways to register watches that are notified when an image's metadata changes:
//! - Synchronously through callbacks using the [`ImageWatchCb`] type;
//! - Asynchronously using the [`ImageWatchAsync`] type.

use std::{
    ffi::c_void,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
};

use futures_util::{Stream, StreamExt, task::AtomicWaker};

use crate::{Result, rbd::Image, util::check_os_error};

#[derive(Debug)]
struct ImageWatchHandle<'a> {
    image: &'a Image<'a>,
    handle: u64,
}

// SAFETY: This would typically be unsound as Image is not Sync.
// However, librbd internally synchronizes the watchers list written to by rbd_update_unwatch, so dropping
// from another thread is fine
unsafe impl Send for ImageWatchHandle<'_> {}
// SAFETY: Since we don't expose the Image reference or any other kind of interior mutability.
unsafe impl Sync for ImageWatchHandle<'_> {}

impl Drop for ImageWatchHandle<'_> {
    fn drop(&mut self) {
        unsafe { rbd_sys::rbd_update_unwatch(self.image.handle().as_ptr(), self.handle) };
    }
}

/// Handle to an callback function that runs when the metadata of an [`Image`] is updated.
///
/// This is a safe API for librbd's `rbd_update_watch`.
///
/// # Example
///
/// ```no_run
/// # async fn test() -> async_ceph::Result<()> {
/// # let mut image: async_ceph::rbd::Image = panic!();
/// use std::sync::{Arc, atomic::{AtomicUsize, Ordering::SeqCst}};
///
/// let update_counter: Arc<AtomicUsize> = Default::default();
///
/// // or ImageWatchCb::new(&image, || { ... })
/// let watch = image.watch_cb(|| {
///     println!("image updated!");
///     update_counter.fetch_add(1, SeqCst);
/// })?;
///
/// // modify the image (here, by resizing it)
/// image.resize(1024, true).await?;
///
/// // unregister the watch
/// drop(watch);
///
/// // the callback has ran and incremented the counter
/// // we should also see "image updated!" printed to stdout
/// assert!(update_counter.load(SeqCst) > 0);
/// # Ok(())
/// # }
/// ```
pub struct ImageWatchCb<'a> {
    _handle: ImageWatchHandle<'a>,
    // SAFETY: The Box<dyn FnMut()> is mutably borrowed by the ImageWatchHandle, so the drop
    // order here is very important! `cb` must not be accessed at all and must be dropped
    // after `handle`.
    _cb: Box<Box<dyn FnMut() + Send>>,
}

// SAFETY: An immutable reference to the callback is not exposed, so even if its concrete type
// is not Sync, the `Sync`ness of the ImageWatchCb is unaffected.
unsafe impl Sync for ImageWatchCb<'_> {}

impl<'a> ImageWatchCb<'a> {
    /// Register a metadata watcher on the provided [`Image`] that will invoke `on_update` when changes are detected.
    pub fn new(image: &'a Image<'_>, on_update: impl FnMut() + Send + 'static) -> Result<Self> {
        let mut cb = Box::new(Box::new(on_update) as Box<dyn FnMut() + Send>);
        let cb_ptr = &raw mut *cb;

        extern "C" fn callback(arg: *mut c_void) {
            // SAFETY:
            // - arg type matches with the one provided to rbd_update_watch call
            // - the mutable reference is unique as the box is not borrowed mutably until the ImageWatchCb
            //   is dropped (at which point the watch has been unregistered)
            let cb = unsafe { &mut *arg.cast::<Box<dyn FnMut() + Send>>() };
            cb()
        }

        let mut handle = 0;
        check_os_error(unsafe {
            rbd_sys::rbd_update_watch(image.handle().as_ptr(), &mut handle, Some(callback), cb_ptr.cast())
        })?;

        Ok(Self {
            _handle: ImageWatchHandle { image, handle },
            _cb: cb,
        })
    }
}

#[derive(Default, Debug)]
struct AsyncWatchShared {
    waker: AtomicWaker,
    update_tx: AtomicUsize,
}

/// Asynchronous [`Stream`] that produces when an [`Image`]'s metadata is updated.
///
/// # Example
///
/// ```no_run
/// # #[cfg(feature = "tokio_rt")]
/// # async fn test() -> async_ceph::Result<()> {
/// # let mut image: async_ceph::rbd::Image = panic!();
///
/// // or ImageWatchAsync::new(&image)
/// let mut watch = image.watch()?;
///
/// // modify the image and wait concurrently on the next watch update
/// // assuming no other client is modifying the image, `watch.wait` complete
/// // resolve right after as librbd updates the image's size
/// let (resize_result, _) = tokio::join!(image.resize(1024, true), watch.wait());
///
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct ImageWatchAsync<'a> {
    _handle: ImageWatchHandle<'a>,
    // SAFETY: This field is mutably borrowed by the handle, so the drop order here is very important!
    // `shared` must not be mutably accessed until drop and must be dropped after `handle`.
    shared: Box<AsyncWatchShared>,
    update_rx: usize,
}

impl<'a> ImageWatchAsync<'a> {
    /// Register a metadata watcher on the provided [`Image`] that can be awaited.
    pub fn new(image: &'a Image<'_>) -> Result<Self> {
        let shared = Box::new(AsyncWatchShared::default());
        let shared_ptr = &raw const *shared;

        extern "C" fn callback(arg: *mut c_void) {
            // SAFETY:
            // - arg type matches with the one provided to rbd_update_watch call
            // - the reference does not alias a mutable one as the box is not borrowed mutably until the ImageWatchCb
            //   is dropped (at which point the watch has been unregistered)
            let shared = unsafe { &*(arg as *const AsyncWatchShared) };

            shared.update_tx.fetch_add(1, Ordering::Relaxed);
            shared.waker.wake();
        }

        let mut handle = 0;
        check_os_error(unsafe {
            rbd_sys::rbd_update_watch(image.handle().as_ptr(), &mut handle, Some(callback), shared_ptr as *mut c_void)
        })?;

        Ok(Self {
            _handle: ImageWatchHandle { image, handle },
            shared,
            update_rx: 0,
        })
    }

    /// Wait until the associated [`Image`]'s metadata is updated.
    ///
    /// This is equivalent to [`Self::next`](futures_util::StreamExt::next), except that the intent is clearer.
    pub async fn wait(&mut self) {
        self.next().await;
    }
}

impl Stream for ImageWatchAsync<'_> {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.shared.waker.register(cx.waker());
        let last_update = self.shared.update_tx.load(Ordering::Relaxed);

        if last_update != self.update_rx {
            self.get_mut().update_rx = last_update;
            Poll::Ready(Some(()))
        } else {
            Poll::Pending
        }
    }
}
