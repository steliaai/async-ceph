// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Abstractions for producing a diff between images of different versions.
//!
//! See the [`DiffIterate` documentation](DiffIterate) for more information.

use std::{
    ffi::{c_int, c_void},
    marker::PhantomData,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures_channel::mpsc;
use futures_util::{Stream, StreamExt};

use crate::{
    async_rt::Executor,
    rbd::{Image, ImageHandle, SnapKey},
    util::check_os_error,
};

/// Marker type indicating that the [`DiffIterate`] will report all allocated extents since
/// the image was first created or cloned from its parent.
///
/// Namely, if the image was cloned from another's snapshot, the extents allocated in the parent before
/// this clone will *not* be reported.
#[derive(Debug)]
#[doc(hidden)]
pub struct FromParent(());

/// Marker type indicating that the [`DiffIterate`] will report all allocated extents since
/// the image, or any of its transitive parents, were first created.
///
/// Unlike [`FromParent`], if this image was cloned from a snapshot this will also report extents
/// allocated by its (transitive) parents.
#[derive(Debug)]
#[doc(hidden)]
pub struct FromCreation(());

/// Marker type indicating that the [`DiffIterate`] will consider the diff since
/// the snapshot with the given id or name.
#[derive(Debug)]
#[doc(hidden)]
pub struct FromSnap(());

#[derive(Debug)]
enum SourceVersion {
    Parent,
    Creation,
    Snap(SnapKey),
}

#[derive(Debug)]
struct DiffIterateParams {
    offset: u64,
    len: u64,
    src: SourceVersion,
    whole_object: bool,
}

#[derive(Debug)]
/// Builder struct used to configure an image diff iteration operation.
pub struct DiffIterate<'a, From = FromParent> {
    image: &'a Image<'a>,
    params: DiffIterateParams,
    _typestate: PhantomData<From>,
}

impl<'a> DiffIterate<'a> {
    /// Create a new [`DiffIterate`] that will report the differences between two versions
    /// of an image.
    ///
    /// # Configuration
    ///
    /// By default, the source version is interpreted as the image state when it was created
    /// or cloned from its parent snapshot, if it has one.
    ///
    /// Note that the latter implies that extents created in the this parent will **not** be returned.
    /// Use [`Self::include_parent`] to include them.
    ///
    /// To restrict the range on which the diff is calculated, a region can be specified through the
    /// [`Self::offset`] and [`Self::len`] methods.
    ///
    /// # Consuming
    ///
    /// The resulting [diff extents](DiffExtent) can be consumed in three ways depending on your use case:
    /// - Collected in a [`Vec`] using [`Self::collect`]. You should use this if you need
    ///   all extents at the same time.
    ///
    /// - Folded over without collecting using [`Self::fold`]. It is more efficient than this over
    ///   [`Self::collect`] if you don't need all extents to be stored in a collection.
    ///
    /// - Streamed asynchronously using [`Self::stream`]. This is idiomatic when async work needs
    ///   to be performed for each extent. However, since librbd's internal `diff_iterate` implementation
    ///   currently collects all extents before exposing any of them to the user, it may be more efficient
    ///   to [`collect`](Self::collect) them before processing.
    ///
    /// # Examples
    ///
    /// Collecting all diff extents into a [`Vec`]:
    ///
    /// ```no_run
    /// # use async_ceph::rbd::{SnapId, diff::DiffIterate};
    /// # async fn test() -> std::io::Result<()> {
    /// # let image: async_ceph::rbd::Image = panic!();
    /// let extents = DiffIterate::new(&image)
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
    ///
    /// Folding over diff extents to calculate the amount of data used by a sparse image:
    ///
    /// ```no_run
    /// # use async_ceph::rbd::{SnapId, diff::DiffIterate};
    /// # async fn test() -> std::io::Result<()> {
    /// # let image: async_ceph::rbd::Image = panic!();
    /// // use diff_iterate to compute the amount of data used by the image
    /// // no snapshot or range specified, will diff entire image from creation
    /// let size = DiffIterate::new(&image)
    ///     .fold(0, |size, diff| if diff.exists { *size += diff.len })
    ///     .await?;
    ///
    /// println!("{size}");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Reading each extent asynchronously as they are reported:
    ///
    /// ```no_run
    /// # use async_ceph::rbd::{SnapId, diff::DiffIterate};
    /// # async fn test() -> std::io::Result<()> {
    /// # let image: async_ceph::rbd::Image = panic!();
    /// use futures_util::StreamExt;
    ///
    /// let mut image_contents = Vec::new();
    ///
    /// // no snapshot or range specified, will diff entire image from creation
    /// let diff_stream = DiffIterate::new(&image).stream();
    ///
    /// while let Some(Ok(extent)) = diff_stream.next().await {
    ///     // ignore zeroed extents
    ///     if !extent.exists {
    ///         continue;
    ///     }
    ///
    ///     // read the extent bytes into a vector
    ///     let buf = Vec::with_capacity(extent.len);
    ///     let (mut buf, n_read) = image.read(extent.offset, buf).await;
    ///     buf.truncate(n_read?);
    ///     image_contents.push(buf);
    /// }
    ///
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// **Note:** This is not more efficient than using [`collect`](Self::collect) first
    /// due to librbd's current `diff_iterate` implementation.
    pub fn new(image: &'a Image<'_>) -> Self {
        Self {
            image,
            params: DiffIterateParams {
                offset: 0,
                len: u64::MAX,
                src: SourceVersion::Parent,
                whole_object: false,
            },
            _typestate: PhantomData,
        }
    }

    /// Only consider changes since the provided snapshot.
    ///
    /// # Notes
    ///
    /// - This must be a snapshot of the current image. It cannot be a snapshot of the image's parent.
    ///
    /// - Unless the `rbd_diff_iterate3` feature is enabled, a [`SnapKey::Id`] will be internally
    ///   converted into a snapshot name before issuing the diff. This may be relevant if renames are
    ///   a possibility.
    pub fn from_snap(self, snap: impl Into<SnapKey>) -> DiffIterate<'a, FromSnap> {
        DiffIterate {
            image: self.image,
            params: DiffIterateParams {
                src: SourceVersion::Snap(snap.into()),
                ..self.params
            },
            _typestate: PhantomData,
        }
    }

    /// Include changes from all of the image's transitive parents.
    ///
    /// # Note
    ///
    /// One effect of this flag that may be unexpected is that any parent regions that were removed/zeroed
    /// in the current image will also be reported. It is not confirmed whether this is intended behavior
    /// or a bug.
    pub fn include_parent(self) -> DiffIterate<'a, FromCreation> {
        DiffIterate {
            image: self.image,
            params: DiffIterateParams {
                src: SourceVersion::Creation,
                ..self.params
            },
            _typestate: PhantomData,
        }
    }
}

impl<'a, From> DiffIterate<'a, From> {
    /// Specify start offset of the diff operation, in bytes.
    ///
    /// By default, this is `0`, i.e. diff from the very start of the image.
    pub fn offset(self, offset: u64) -> Self {
        Self {
            params: DiffIterateParams { offset, ..self.params },
            ..self
        }
    }

    /// Specify the length of the contiguous region that will diffed, in bytes.
    ///
    /// By default this is [`u64::MAX`], i.e. diff the entire image.
    pub fn len(self, len: u64) -> Self {
        Self {
            params: DiffIterateParams { len, ..self.params },
            ..self
        }
    }

    /// Specify that the diff should be limited to the extents of a full object instead of
    /// showing intra-object deltas.
    ///
    /// When the object map feature is enabled on an image, limiting the diff to the object
    /// extents will dramatically improve performance since the differences can be computed by
    /// examining the in-memory object map instead of querying RADOS for each object within
    /// the image.
    pub fn whole_object(self) -> Self {
        Self {
            params: DiffIterateParams {
                whole_object: true,
                ..self.params
            },
            ..self
        }
    }

    /// Perform a diff with the current configuration, invoking a callback on every diff extent
    /// along with a mutable state, returning said state once all extents have been processed.
    ///
    /// This has less overhead than [`Self::stream`], and should be preferred if being
    /// in an async context for each individual reported extent is not necessary.
    ///
    /// If you need to collect all extents into a vector, prefer [`Self::collect`] for terseness.
    ///
    /// See the [type level documentation](DiffIterate) for a usage example.
    pub async fn fold<B, F>(self, init: B, mut f: F) -> crate::Result<B>
    where
        B: Send + 'static,
        F: FnMut(&mut B, DiffExtent) + Send + 'static, {
        let handle = self.image.handle().clone();
        let params = self.params;

        let state = Arc::new(Mutex::new(init));
        let state_clone = state.clone();

        let result = self
            .image
            .executor
            .spawn_blocking(move || {
                // Panic safety: this mutex has no contention.
                let state = &mut *state_clone.try_lock().unwrap();
                diff_iterate_internal(&handle, params, move |diff| f(state, diff))
            })
            .await
            .expect("failed to await blocking task");

        result.map(move |_| {
            Arc::into_inner(state)
                .expect("Arc should have been unique")
                .into_inner()
                .expect("state poisonned")
        })
    }

    /// Perform a diff with the current configuration, collecting all modified extents into a
    /// [`Vec`].
    ///
    /// # Alternatives
    ///
    /// If you don't need to store all the extents in a collection, [`Self::fold`]
    /// will be more CPU and memory efficient.
    ///
    /// See the [type level documentation](DiffIterate) for a usage example.
    pub async fn collect(self) -> crate::Result<Vec<DiffExtent>> {
        self.fold(Vec::new(), |vec, extent| vec.push(extent)).await
    }

    /// Perform a diff with the current configuration, creating an asynchronous [`Stream`] of
    /// [`DiffExtent`]s.
    ///
    /// Note that the current librbd implementation for `diff_iterate` will first collect and process
    /// all extents, and then report all of them at once. Hence [`Self::fold`] is generally preferred.
    /// However, this may change in future versions.
    ///
    /// See the [type level documentation](DiffIterate) for a usage example.
    pub fn stream(self) -> DiffStream<'a> {
        DiffStream(DiffStreamState::Created {
            image: self.image,
            params: self.params,
        })
    }
}

/// Represents an extent of the image that changed between two of its versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffExtent {
    /// The offset the extent.
    pub offset: u64,
    /// The size of the extent, in bytes.
    pub len: usize,
    /// Whether the extent is populated with data.
    ///
    /// If false, the extent is known/defined to be zeros.
    pub exists: bool,
}

#[derive(Debug)]
enum DiffStreamState<'a> {
    Created {
        image: &'a Image<'a>,
        params: DiffIterateParams,
    },
    Submitted(futures_channel::mpsc::UnboundedReceiver<crate::Result<DiffExtent>>),
    // Panicked when spawning the blocking task
    Poisoned,
}

/// An asynchronous [`Stream`] that reports differences between two versions of an
/// [`Image`].
#[must_use = "DiffStream is a stream and does nothing unless `.poll_next` or `.next` is called"]
#[derive(Debug)]
pub struct DiffStream<'a>(DiffStreamState<'a>);

impl Stream for DiffStream<'_> {
    type Item = crate::Result<DiffExtent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let state = &mut self.0;
        if let DiffStreamState::Created { .. } = state {
            let DiffStreamState::Created { image, params } = std::mem::replace(state, DiffStreamState::Poisoned) else {
                unreachable!()
            };

            let (tx, rx) = mpsc::unbounded();
            let handle = image.handle().clone();
            let _task = image.executor.spawn_blocking(move || {
                let tx_ref = &tx;
                let res = diff_iterate_internal(&handle, params, move |diff| {
                    let _ = tx_ref.unbounded_send(Ok(diff));
                });
                if let Err(e) = res {
                    let _ = tx.unbounded_send(Err(e));
                }
            });

            *state = DiffStreamState::Submitted(rx);
        }

        let DiffStreamState::Submitted(rx) = state else {
            panic!("DiffStream poisoned")
        };
        rx.poll_next_unpin(cx)
    }
}

/// Safe blocking wrapper around `rbd_iterate2` and `rbd_iterate3` (depending on the type of snapshot
/// identifier provided).
fn diff_iterate_internal<F>(image: &ImageHandle, params: DiffIterateParams, mut callback: F) -> crate::Result<()>
where
    F: FnMut(DiffExtent) + Send, {
    unsafe extern "C" fn ffi_callback<F>(offset: u64, len: usize, exists: c_int, arg: *mut c_void) -> c_int
    where
        F: FnMut(DiffExtent) + Send, {
        let callback: &mut F = unsafe { &mut *arg.cast() };
        callback(DiffExtent {
            offset,
            len,
            exists: exists != 0,
        });
        0
    }

    // librbd adds `offset` and `len` without handling overflow. If this happens, the function
    // hangs due to a loop exit condition never being resolved!
    let clamped_len = params.len.min(u64::MAX - params.offset);

    let (snap_id, mut flags) = match params.src {
        SourceVersion::Creation => (0, rbd_sys::RBD_DIFF_ITERATE_FLAG_INCLUDE_PARENT),
        SourceVersion::Parent => (0, 0),
        SourceVersion::Snap(SnapKey::Id(snap_id)) => (snap_id.0, 0),
        SourceVersion::Snap(SnapKey::Name(name)) => {
            check_os_error(unsafe {
                rbd_sys::rbd_diff_iterate2(
                    image.as_ptr(),
                    name.as_ptr(),
                    params.offset,
                    clamped_len,
                    0,
                    params.whole_object as u8,
                    Some(ffi_callback::<F>),
                    (&raw mut callback).cast(),
                )
            })?;

            return Ok(());
        }
    };

    if params.whole_object {
        flags |= rbd_sys::RBD_DIFF_ITERATE_FLAG_WHOLE_OBJECT;
    }

    #[cfg(feature = "rbd_diff_iterate3")]
    check_os_error(unsafe {
        rbd_sys::rbd_diff_iterate3(
            image.as_ptr(),
            snap_id,
            params.offset,
            clamped_len,
            flags,
            Some(ffi_callback::<F>),
            (&raw mut callback).cast(),
        )
    })?;

    // polyfill for rbd_diff_iterate3
    #[cfg(not(feature = "rbd_diff_iterate3"))]
    {
        use crate::rbd::snap::{AsSnapKey, SnapId};

        let mut name = None;
        let id = SnapId(snap_id);
        if snap_id != 0 {
            name = Some(id.name(image)?);
        }
        let name_ptr = name.map_or(std::ptr::null(), |n| n.as_ptr());

        check_os_error(unsafe {
            rbd_sys::rbd_diff_iterate2(
                image.as_ptr(),
                name_ptr,
                params.offset,
                clamped_len,
                ((flags & rbd_sys::RBD_DIFF_ITERATE_FLAG_INCLUDE_PARENT) != 0) as u8,
                params.whole_object as u8,
                Some(ffi_callback::<F>),
                (&raw mut callback).cast(),
            )
        })?;
    }

    Ok(())
}
