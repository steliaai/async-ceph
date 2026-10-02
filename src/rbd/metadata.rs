// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! RBD image metadata types.

use std::{
    ffi::{CStr, CString},
    io,
    mem::ManuallyDrop,
    time::SystemTime,
};

use libc::c_int;
use rbd_sys::rbd_image_t;

use crate::{
    Result,
    rbd::{Features, OpFeatures, snap::SnapSpec},
    util::{self, check_os_error, get_known_size_arr, get_known_size_cstr, get_unknown_size_cstr},
};

/// The name and unique ID of an [`Image`](super::Image).
#[repr(transparent)]
pub struct ImageSpec {
    inner: rbd_sys::rbd_image_spec_t,
}

unsafe impl Send for ImageSpec {}
unsafe impl Sync for ImageSpec {}

impl ImageSpec {
    pub fn name(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.inner.name) }
    }

    pub fn id(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.inner.id) }
    }
}

impl std::fmt::Debug for ImageSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageSpec").field("name", &self.name()).field("id", &self.id()).finish()
    }
}

impl Drop for ImageSpec {
    fn drop(&mut self) {
        unsafe { rbd_sys::rbd_image_spec_cleanup(&mut self.inner) };
    }
}

/// Internal getter for implementing [`ImageMeta`].
pub(super) trait Sealed: Sized + Send + 'static {
    unsafe fn get(image: rbd_image_t) -> crate::Result<Self>;
}

/// Trait implemented on image metadata types.
///
/// Note that if the metadata type is optionally absent, the trait will be implemented on [`Option<T>`].
///
/// Types implementing this can be fetched using [`Image::get_metadata`](super::Image::get_metadata).
#[allow(private_bounds)]
pub trait ImageMeta: Sealed {}

impl Sealed for super::Features {
    unsafe fn get(image: rbd_image_t) -> Result<Self> {
        let mut features = Self::empty();
        check_os_error(unsafe { rbd_sys::rbd_get_features(image, (&raw mut features).cast()) })?;
        Ok(features)
    }
}
impl ImageMeta for super::Features {}

impl Sealed for super::OpFeatures {
    unsafe fn get(image: rbd_image_t) -> Result<Self> {
        let mut op_features = Self::empty();
        check_os_error(unsafe { rbd_sys::rbd_get_op_features(image, (&raw mut op_features).cast()) })?;
        Ok(op_features)
    }
}
impl ImageMeta for super::OpFeatures {}

/// Basic [`Image`](super::Image) information.
#[derive(Debug, Clone)]
pub struct ImageStat {
    /// Data size of the image.
    pub size: u64,
    /// Full size taken by the image objects.
    pub obj_size: u64,
    /// Number of objects.
    pub num_objs: u64,
    /// Image order.
    pub order: c_int,
}

impl Sealed for ImageStat {
    unsafe fn get(image: rbd_image_t) -> Result<Self> {
        let mut stat = unsafe { std::mem::zeroed() };
        check_os_error(unsafe { rbd_sys::rbd_stat(image, &mut stat, size_of_val(&stat)) })?;

        Ok(Self {
            size: stat.size,
            obj_size: stat.obj_size,
            num_objs: stat.num_objs,
            order: stat.order,
        })
    }
}
impl ImageMeta for ImageStat {}

pub(crate) fn to_system_time(ts: rbd_sys::timespec) -> crate::Result<std::time::SystemTime> {
    util::to_system_time(ts.tv_sec as u64, ts.tv_nsec as u64)
}

/// Creation, access and modification times of an [`Image`](super::Image).
#[derive(Debug, Clone)]
pub struct ImageTimestamps {
    pub accessed: SystemTime,
    pub created: SystemTime,
    pub modified: SystemTime,
}

impl Default for ImageTimestamps {
    fn default() -> Self {
        use std::time::UNIX_EPOCH;
        Self {
            created: UNIX_EPOCH,
            accessed: UNIX_EPOCH,
            modified: UNIX_EPOCH,
        }
    }
}

impl Sealed for ImageTimestamps {
    unsafe fn get(image: rbd_image_t) -> Result<Self> {
        let mut created = rbd_sys::timespec { tv_sec: 0, tv_nsec: 0 };
        let mut accessed = rbd_sys::timespec { tv_sec: 0, tv_nsec: 0 };
        let mut modified = rbd_sys::timespec { tv_sec: 0, tv_nsec: 0 };

        check_os_error(unsafe { rbd_sys::rbd_get_create_timestamp(image, &mut created) })?;
        check_os_error(unsafe { rbd_sys::rbd_get_access_timestamp(image, &mut accessed) })?;
        check_os_error(unsafe { rbd_sys::rbd_get_modify_timestamp(image, &mut modified) })?;

        Ok(Self {
            created: to_system_time(created)?,
            accessed: to_system_time(accessed)?,
            modified: to_system_time(modified)?,
        })
    }
}
impl ImageMeta for ImageTimestamps {}

/// Information about the group an image belongs to.
///
/// Note that the [`ImageMeta`] implementation is on `Option<ImageGroup>`, since
/// an image may not be part of a group.
#[derive(Debug, Clone)]
pub struct ImageGroup {
    pub name: CString,
    pub pool: u64,
}

impl Sealed for Option<ImageGroup> {
    unsafe fn get(image: rbd_image_t) -> Result<Self> {
        let mut ginfo: rbd_sys::rbd_group_info_t = unsafe { std::mem::zeroed() };
        check_os_error(unsafe { rbd_sys::rbd_get_group(image, &mut ginfo, size_of_val(&ginfo)) })?;

        // instead of returning ENOENT as is usually done when librbd is asked for something that
        // doesn't exist, rbd_get_group returns a sentinel value with pool == -1.
        let image_group = (ginfo.pool != -1).then(|| ImageGroup {
            name: unsafe { CStr::from_ptr(ginfo.name).to_owned() },
            pool: ginfo.pool as u64,
        });

        unsafe { rbd_sys::rbd_group_info_cleanup(&mut ginfo, size_of_val(&ginfo)) };
        Ok(image_group)
    }
}
impl ImageMeta for Option<ImageGroup> {}

/// Description of an [`Image`](super::Image)'s parent or descendant.
#[repr(transparent)]
pub struct LinkedImageSpec {
    inner: rbd_sys::rbd_linked_image_spec_t,
}

unsafe impl Send for LinkedImageSpec {}
unsafe impl Sync for LinkedImageSpec {}

impl Drop for LinkedImageSpec {
    fn drop(&mut self) {
        unsafe { rbd_sys::rbd_linked_image_spec_cleanup(&mut self.inner) };
    }
}

impl std::fmt::Debug for LinkedImageSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkedImageSpec")
            .field("pool_id", &self.pool_id())
            .field("pool_name", &self.pool_name())
            .field("pool_namespace", &self.pool_namespace())
            .field("image_id", &self.image_id())
            .field("image_name", &self.image_name())
            .field("trash", &self.trash())
            .finish()
    }
}

impl LinkedImageSpec {
    /// ID of the pool the linked image is stored in.
    pub fn pool_id(&self) -> u64 {
        self.inner.pool_id as u64
    }

    /// Name of the pool the linked image is stored in.
    pub fn pool_name(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.inner.pool_name) }
    }

    /// Namespace of the pool the linked image is stored in.
    pub fn pool_namespace(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.inner.pool_namespace) }
    }

    /// ID of the linked image.
    pub fn image_id(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.inner.image_id) }
    }

    /// Name of the linked image.
    pub fn image_name(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.inner.image_name) }
    }

    /// Whether this linked image has been moved to the trash.
    pub fn trash(&self) -> bool {
        self.inner.trash
    }
}

/// Information about an image's parent.
///
/// Note that the [`ImageMeta`] implementation is on `Option<ImageParent>`, since
/// an image may not have any parent.
#[derive(Debug)]
pub struct ImageParent {
    /// Information about the parent's image.
    pub image: LinkedImageSpec,
    /// Information about the parent's snapshot.
    pub snap: SnapSpec,
}

impl Sealed for Option<ImageParent> {
    unsafe fn get(image: rbd_image_t) -> Result<Self> {
        let mut parent: ManuallyDrop<ImageParent> = unsafe { std::mem::zeroed() };
        let errno = unsafe { rbd_sys::rbd_get_parent(image, &mut parent.image.inner, &mut parent.snap.inner) };
        match check_os_error(errno) {
            Ok(_) => Ok(Some(ManuallyDrop::into_inner(parent))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
impl ImageMeta for Option<ImageParent> {}

/// Information about an image's direct children.
#[derive(Debug, Default)]
pub struct ImageChildren(pub Vec<LinkedImageSpec>);

impl Sealed for ImageChildren {
    unsafe fn get(image: rbd_image_t) -> Result<Self> {
        // SAFETY: by #[repr(transparent)] of LinkedImageSpec
        Ok(Self(unsafe {
            get_known_size_arr(|buf: *mut LinkedImageSpec, size| rbd_sys::rbd_list_children3(image, buf.cast(), size), 16)
        }?))
    }
}
impl ImageMeta for ImageChildren {}

/// Information about an image's descendents, i.e. all transitive children.
#[derive(Debug, Default)]
pub struct ImageDescendants(pub Vec<LinkedImageSpec>);

impl Sealed for ImageDescendants {
    unsafe fn get(image: rbd_image_t) -> Result<Self> {
        // SAFETY: by #[repr(transparent)] of LinkedImageSpec
        Ok(Self(unsafe {
            get_known_size_arr(|buf: *mut LinkedImageSpec, size| rbd_sys::rbd_list_descendants(image, buf.cast(), size), 16)
        }?))
    }
}
impl ImageMeta for ImageDescendants {}

/// Information about a RBD image watch that is possibly owned by a remote client.
#[repr(transparent)]
pub struct ImageWatcher {
    inner: rbd_sys::rbd_image_watcher_t,
}

unsafe impl Send for ImageWatcher {}
unsafe impl Sync for ImageWatcher {}

impl ImageWatcher {
    /// Remote address of the client holding the watch.
    ///
    /// The format is `<ip>:<port>/<internal id>`. Note that said ID is not the same as [`Self::id`]!
    pub fn addr(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.inner.addr) }
    }

    /// Unique ID of this watch.
    pub fn id(&self) -> i64 {
        self.inner.id
    }

    pub fn cookie(&self) -> u64 {
        self.inner.cookie
    }
}

impl core::fmt::Debug for ImageWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageWatcher")
            .field("addr", &self.addr())
            .field("id", &self.id())
            .field("cookie", &self.cookie())
            .finish()
    }
}

impl Drop for ImageWatcher {
    fn drop(&mut self) {
        unsafe { rbd_sys::rbd_watchers_list_cleanup(&mut self.inner, 1) };
    }
}

/// Information about the watchers registered to an image.
#[derive(Debug, Default)]
pub struct ImageWatchers(pub Vec<ImageWatcher>);

impl Sealed for ImageWatchers {
    unsafe fn get(image: rbd_sys::rbd_image_t) -> Result<Self> {
        // SAFETY: by #[repr(transparent)] of ImageWatcher
        Ok(Self(unsafe {
            get_known_size_arr(|buf: *mut ImageWatcher, size| rbd_sys::rbd_watchers_list(image, buf.cast(), size), 16)
        }?))
    }
}
impl ImageMeta for ImageWatchers {}

/// Detailed image information returned by [`Image::full_info`](super::Image::full_info).
#[derive(Default, Debug, Clone)]
pub struct ImageFullInfo {
    /// Data size of the image.
    pub size: u64,
    /// Full size taken by the image objects.
    pub obj_size: u64,
    /// Number of objects.
    pub num_objs: u64,
    /// Image order.
    pub order: c_int,
    /// Whether this image is stored using the old image format.
    pub old_format: bool,
    /// Image features.
    pub features: Features,
    /// Image operation features.
    pub op_features: OpFeatures,
    /// Image stripe unit.
    pub stripe_unit: u64,
    /// Image stripe count.
    pub stripe_count: u64,
    /// Image timestamps.
    pub timestamps: ImageTimestamps,
    /// Image overlap.
    pub overlap: u64,
    /// Image name, as a C string.
    pub name: CString,
    /// Image ID, as a C string.
    pub id: CString,
    /// Image block name prefix, as a C string.
    pub block_name_prefix: CString,
    /// Image data pool id.
    pub data_pool_id: u64,
    /// Image flags.
    pub flags: u64,
}

impl Sealed for ImageFullInfo {
    unsafe fn get(image: rbd_image_t) -> Result<Self> {
        let mut info = ImageFullInfo::default();

        let stat = unsafe { ImageStat::get(image) }?;
        info.size = stat.size;
        info.obj_size = stat.obj_size;
        info.num_objs = stat.num_objs;
        info.order = stat.order;

        let mut old_format = 0;
        check_os_error(unsafe { rbd_sys::rbd_get_old_format(image, &mut old_format) })?;
        info.old_format = old_format != 0;

        info.features = unsafe { Features::get(image)? };
        info.op_features = unsafe { OpFeatures::get(image)? };

        check_os_error(unsafe { rbd_sys::rbd_get_stripe_unit(image, &mut info.stripe_unit) })?;
        check_os_error(unsafe { rbd_sys::rbd_get_stripe_count(image, &mut info.stripe_count) })?;

        info.timestamps = unsafe { ImageTimestamps::get(image)? };

        check_os_error(unsafe { rbd_sys::rbd_get_overlap(image, &mut info.overlap) })?;

        // The (deprecated) max name size constant is set to 96 bytes, but in practice they're very
        // likely to be shorter. 32 is probably a good initial capacity
        info.name = unsafe { get_known_size_cstr(|buf, size| rbd_sys::rbd_get_name(image, buf, size), 32)? };
        // IDs are typically 12 bytes, so 16 is a good capacity
        info.id = unsafe { get_unknown_size_cstr(|buf, size| rbd_sys::rbd_get_id(image, buf, size), 16)? };
        // The deprecated max block prefix size is 24 bytes, but they are now bigger.
        // 32 should be a good initial capacity
        info.block_name_prefix =
            unsafe { get_unknown_size_cstr(|buf, size| rbd_sys::rbd_get_block_name_prefix(image, buf, size), 32)? };

        info.data_pool_id = check_os_error(unsafe { rbd_sys::rbd_get_data_pool_id(image) })?;

        Ok(info)
    }
}
impl ImageMeta for ImageFullInfo {}
