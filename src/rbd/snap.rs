// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Image snapshot APIs.
//!
//! Most functionality is accessible as methods on the [`Snapshot`] type, which represents a
//! reference to an image snapshot by some identifier (name or unique ID).
//!
//! # Identifiers
//!
//! RBD snapshots can be identified by their name (which is unique but mutable) or through an
//! immutable 64-bit unique ID. This causes API friction as many [`rbd_sys`] functions only
//! accept one of the two.
//!
//! To solve this, the [`Snapshot`] type seamlessly converts between name and ID as required.
//! This can be an important consideration in environments where images can be renamed concurrently;
//! as such methods that may require these conversions specify which kind of image identifier is
//! natively accepted by librbd in their documentation.

use std::{
    borrow::Cow,
    convert::Infallible,
    ffi::{CStr, CString, c_int},
    io::ErrorKind,
    mem::MaybeUninit,
    ops::Deref,
    time::SystemTime,
};

use rbd_sys::rbd_snap_namespace_type_t;
use tracing::field;

use crate::{
    rbd::{Image, ImagePtr, ProgressOp, metadata},
    util::{self, TryIntoCString, check_os_error},
};

/// A unique 64-bit snapshot identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SnapId(pub u64);

impl SnapId {
    /// Special snapshot ID that refers to the live, writable image (i.e. no snapshot).
    pub const HEAD: Self = SnapId(u64::MAX - 1);

    /// Check if this snapshot ID refers to the live, writable image.
    pub const fn is_head(&self) -> bool {
        self.0 == Self::HEAD.0
    }
}

impl std::fmt::Debug for SnapId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_head() {
            f.write_str("SnapId::HEAD")
        } else {
            f.debug_tuple("SnapId").field(&self.0).finish()
        }
    }
}

impl std::fmt::Display for SnapId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_head() {
            f.write_str("HEAD")
        } else {
            std::fmt::Display::fmt(&self.0, f)
        }
    }
}

impl From<u64> for SnapId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<SnapId> for u64 {
    fn from(value: SnapId) -> Self {
        value.0
    }
}

/// Key that uniquely identifies a snapshot in an RBD pool.
///
/// This is either the snapshot's name (which is unique but mutable), or
/// its unique ID (which is immutable).
#[derive(Debug, Clone)]
pub enum SnapKey {
    /// Unique 64-bit identifier.
    Id(SnapId),
    /// Name of the snapshot, as a C string.
    Name(CString),
}

impl From<u64> for SnapKey {
    fn from(value: u64) -> Self {
        SnapKey::Id(SnapId(value))
    }
}

impl From<SnapId> for SnapKey {
    fn from(value: SnapId) -> Self {
        SnapKey::Id(value)
    }
}

// Ideally we would impl TryFrom<T> for SnapKey where T: TryIntoCString,
// but this conflicts with the From<u64> impl due to std's blanket Into -> TryFrom.
// So the best we can do is implement From for infallible conversions.
impl<T: TryIntoCString<Error = Infallible>> From<T> for SnapKey {
    fn from(value: T) -> Self {
        let Ok(name) = value.try_into_cstring();
        SnapKey::Name(name)
    }
}

impl SnapKey {
    /// Special snapshot ID that refers to the head/live image (i.e. no snapshot).
    pub const HEAD: Self = Self::Id(SnapId::HEAD);

    /// Create a [`SnapKey::Name`] with a string representation that may fail to be
    /// converted to a [`CString`].
    pub fn name<T: TryIntoCString>(name: T) -> Result<Self, T::Error> {
        Ok(SnapKey::Name(name.try_into_cstring()?))
    }
}

pub(super) trait AsSnapKey: Into<SnapKey> + std::fmt::Debug + Clone + Send + Sync + 'static {
    fn name(&self, image: &ImagePtr) -> crate::Result<Cow<'_, CStr>>;

    fn id(&self, image: &ImagePtr) -> crate::Result<SnapId>;

    fn rename(&mut self, #[allow(unused)] name: CString) {}

    fn as_snap_key(&self) -> Cow<'_, SnapKey> {
        Cow::Owned(self.clone().into())
    }
}

impl AsSnapKey for SnapId {
    fn id(&self, _image: &ImagePtr) -> crate::Result<SnapId> {
        Ok(*self)
    }

    fn name(&self, image: &ImagePtr) -> crate::Result<Cow<'_, CStr>> {
        let cstr =
            unsafe { util::get_known_size_cstr(|buf, size| rbd_sys::rbd_snap_get_name(image.as_ptr(), self.0, buf, size), 32)? };
        Ok(Cow::Owned(cstr))
    }
}

impl AsSnapKey for CString {
    fn id(&self, image: &ImagePtr) -> crate::Result<SnapId> {
        let mut id = 0;
        check_os_error(unsafe { rbd_sys::rbd_snap_get_id(image.as_ptr(), self.as_ptr(), &mut id) })?;
        Ok(SnapId(id))
    }

    fn name(&self, _image: &ImagePtr) -> crate::Result<Cow<'_, CStr>> {
        Ok(Cow::Borrowed(self))
    }

    fn rename(&mut self, name: CString) {
        *self = name;
    }
}

impl AsSnapKey for SnapKey {
    fn id(&self, image: &ImagePtr) -> crate::Result<SnapId> {
        match self {
            Self::Id(id) => Ok(*id),
            Self::Name(name) => name.id(image),
        }
    }

    fn name(&self, image: &ImagePtr) -> crate::Result<Cow<'_, CStr>> {
        match self {
            Self::Name(name) => Ok(Cow::Borrowed(name)),
            Self::Id(id) => id.name(image),
        }
    }

    fn rename(&mut self, name: CString) {
        if let Self::Name(old_name) = self {
            *old_name = name;
        }
    }

    fn as_snap_key(&self) -> Cow<'_, SnapKey> {
        Cow::Borrowed(self)
    }
}

/// A RBD snapshot object, identified by its name or unique ID.
///
/// The `Key` generic parameter may be a [`SnapKey`], a [`SnapId`] or a [`CString`]. When calling a method
/// that requires a different kind of identifier, the other is transparently queried.
///
/// # Effect of renames
///
/// What this [`Snapshot`] object points to after [`rename`](Snapshot::rename) is called depends on
/// the identifier type:
///
/// - When referred to by name, the [`Snapshot`] on which [`rename`](Snapshot::rename) was called will be
///   updated. Other [`Snapshot`] objects that used to point to the old name are **not** updated.
///
/// - When referred to by ID, [`Snapshot`] objects still point to the same, now renamed one.
#[allow(private_bounds)]
#[derive(Debug)]
pub struct Snapshot<'a, Key: AsSnapKey = SnapKey> {
    image: &'a Image<'a>,
    key: Key,
}

/// Snapshot identified by name.
///
/// This is a type alias. See [the parent documentation](Snapshot) for more details.
pub type SnapshotByName<'a> = Snapshot<'a, CString>;

/// Snapshot identified by its unique integer ID.
///
/// This is a type alias. See [the parent documentation](Snapshot) for more details.
pub type SnapshotById<'a> = Snapshot<'a, SnapId>;

#[allow(private_bounds)]
impl<'a, Key: AsSnapKey> Snapshot<'a, Key> {
    /// Get a reference to the [`Image`] associated with this [`Snapshot`].
    pub fn image(&self) -> &'a Image<'a> {
        self.image
    }

    /// Get this snapshot identifier as a [`SnapKey`].
    pub fn as_snap_key(&self) -> Cow<'_, SnapKey> {
        self.key.as_snap_key()
    }

    /// Check if this snapshot exists.
    #[tracing::instrument("Snapshot::exists", level = "debug", err)]
    pub async fn exists(&self) -> crate::Result<bool> {
        let key = self.key.as_snap_key();
        match &*key {
            // This snapshot is identified by unique ID. Try to get its name and check for NotFound.
            SnapKey::Id(id) => {
                let id = *id;
                self.image
                    .do_blocking(move |image| match id.name(&image) {
                        Ok(_) => Ok(true),
                        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
                        Err(e) => Err(e),
                    })
                    .await
            }
            // This snapshot is identified by name. Try to get its ID and check for NotFound.
            // this is supported on librbd 1.15, while rbd_snap_exists isn't.
            SnapKey::Name(_) => {
                // This second match avoids cloning the name a second time if Id is CString.
                let name = match key.into_owned() {
                    SnapKey::Id(_) => unreachable!(),
                    SnapKey::Name(name) => name,
                };
                self.image
                    .do_blocking(move |image| match name.id(&image) {
                        Ok(_) => Ok(true),
                        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
                        Err(e) => Err(e),
                    })
                    .await
            }
        }
    }

    // TODO: Ideally this should take self by value and return a Result<(), RemoveError<'a, Key>>
    // so that reusing a deleted snapshot is impossible while also letting it be reclaimed on error.
    //
    // However ProgressOp is limited to 'static return types. Some kind of MappedProgressOp<'_, R, M>
    // that maps R into a different non 'static type M would be required.

    /// Delete this snapshot by name.
    ///
    /// For this to succeed, the snapshot must not be protected and all child images must have been
    /// flattened. This is already the case if the deep flatten image feature was enabled.
    ///
    /// Note that this method identifies the snapshot *by name*. If called on a [`SnapshotById`], the
    /// name will be transparently queried first. This is important to consider if the image could
    /// be concurrently renamed.
    pub fn remove(&self, flags: SnapRemoveFlags) -> ProgressOp<'a, crate::Result<()>> {
        let span = tracing::debug_span!("Snapshot::remove", ?self, ?flags);

        let key = self.key.clone();
        let image = self.image.handle().clone();

        ProgressOp::new(self.image.executor, move |cb, data| unsafe {
            let _enter = span.enter();
            let name = key.name(&image)?;

            check_os_error(rbd_sys::rbd_snap_remove2(image.as_ptr(), name.as_ptr(), flags.bits(), cb, data))
                .inspect_err(|err| tracing::error!("rbd_snap_remove2 failed: {err}"))?;
            Ok(())
        })
    }

    /// Check if this snapshot is currently protected against deletion.
    ///
    /// Note that this method identifies the snapshot *by name*. If called on a [`SnapshotById`], the
    /// name will be transparently queried first. This is important to consider if the image could
    /// be concurrently renamed.
    #[tracing::instrument("Snapshot::is_protected", level = "debug", err)]
    pub async fn is_protected(&self) -> crate::Result<bool> {
        let key = self.key.clone();
        self.image
            .do_blocking(move |image| unsafe {
                let name = key.name(&image)?;
                let mut protected = 0;
                check_os_error(rbd_sys::rbd_snap_is_protected(image.as_ptr(), name.as_ptr(), &mut protected))?;
                Ok(protected != 0)
            })
            .await
    }

    /// Protect or unprotect this snapshot. A protected snapshot cannot be deleted until it is unprotected.
    ///
    /// On success, returns whether the snapshot was previously protected.
    ///
    /// Note that this method identifies the snapshot *by name*. If called on a [`SnapshotById`], the
    /// name will be transparently queried first. This is important to consider if the image could
    /// be concurrently renamed.
    #[tracing::instrument("Snapshot::protect", level = "debug", err)]
    pub async fn protect(&self, protected: bool) -> crate::Result<bool> {
        let key = self.key.clone();
        let ffi = match protected {
            true => rbd_sys::rbd_snap_protect,
            false => rbd_sys::rbd_snap_unprotect,
        };

        self.image
            .do_blocking(move |image| unsafe {
                let name = key.name(&image)?;
                match check_os_error(ffi(image.as_ptr(), name.as_ptr())) {
                    Ok(_) => Ok(!protected),
                    Err(e) if protected && e.kind() == ErrorKind::ResourceBusy => Ok(true),
                    Err(e) if !protected && e.kind() == ErrorKind::InvalidInput => Ok(false),
                    Err(e) => Err(e.into()),
                }
            })
            .await
    }

    /// Rename this snapshot to `new_name`.
    ///
    /// If this [`Snapshot`] object was identified by name, it will point to the new name after a successful rename.
    ///
    /// Note that this method identifies the snapshot *by its original name*. If called on a [`SnapshotById`],
    /// the current name will be transparently queried first. This is important to consider if the image could
    /// be concurrently renamed.
    #[tracing::instrument("Snapshot::rename", level = "debug", err, fields(new_name = field::Empty))]
    pub async fn rename(&mut self, new_name: impl TryIntoCString) -> crate::Result<()> {
        let new_name = new_name.try_into_cstring().map_err(crate::Error::ffi)?;
        tracing::Span::current().record("new_name", field::debug(&new_name));

        let key = self.key.clone();
        self.image
            .do_blocking(move |image| unsafe {
                let name = key.name(&image)?;
                check_os_error(rbd_sys::rbd_snap_rename(image.as_ptr(), name.as_ptr(), new_name.as_ptr()))?;
                Ok(new_name)
            })
            .await
            .map(|new_name| self.key.rename(new_name))
    }

    /// Get the creation timestamp for this snapshot.
    ///
    /// Note that this method identifies the snapshot *by its ID*. If called on a [`SnapshotByName`],
    /// the ID will be transparently queried first.
    #[tracing::instrument("Snapshot::timestamp", level = "debug", err)]
    pub async fn timestamp(&self) -> crate::Result<SystemTime> {
        let key = self.key.clone();
        self.image
            .do_blocking(move |image| unsafe {
                let id = key.id(&image)?;
                let mut timespec = MaybeUninit::uninit();
                check_os_error(rbd_sys::rbd_snap_get_timestamp(image.as_ptr(), id.0, timespec.as_mut_ptr()))?;
                metadata::to_system_time(timespec.assume_init())
            })
            .await
    }

    /// Get this snapshot's namespace type.
    ///
    /// Note that this method identifies the snapshot *by its ID*. If called on a [`SnapshotByName`],
    /// the ID will be transparently queried first.
    #[tracing::instrument("Snapshot::namespace_type", level = "debug", err)]
    pub async fn namespace_type(&self) -> crate::Result<SnapNamespaceType> {
        let key = self.key.clone();
        self.image
            .do_blocking(move |image| unsafe {
                let id = key.id(&image)?;
                let mut ns = rbd_sys::rbd_snap_namespace_type_t::RBD_SNAP_NAMESPACE_TYPE_GROUP;
                check_os_error(rbd_sys::rbd_snap_get_namespace_type(image.as_ptr(), id.0, &mut ns))?;
                Ok(ns.into())
            })
            .await
    }

    /// Get information about this snapshot's namespace.
    ///
    /// Note that this method identifies the snapshot *by its ID*. If called on a [`SnapshotByName`],
    /// the ID will be transparently queried first.
    #[tracing::instrument("Snapshot::namespace", level = "debug", err)]
    pub async fn namespace(&self) -> crate::Result<SnapNamespace> {
        let key = self.key.clone();
        self.image
            .do_blocking(move |image| unsafe {
                let id = key.id(&image)?;
                SnapNamespace::new(image.as_ptr(), id)
            })
            .await
    }
}

impl<'a> Snapshot<'a, SnapId> {
    /// Create a [`Snapshot`] instance identifying the snapshot through its unique ID.
    ///
    /// Note that this does not check whether the snapshot exists. To do so you must call [`Snapshot::exists`].
    pub fn from_id(image: &'a Image<'a>, id: SnapId) -> Self {
        Snapshot { image, key: id }
    }

    /// Get the unique, immutable ID of the snapshot.
    pub fn id(&self) -> SnapId {
        self.key
    }

    /// Get the unique name of the snapshot.
    ///
    /// Since this [`Snapshot`] object is identified by ID, this must be queried.
    pub async fn name(&self) -> crate::Result<CString> {
        let id = self.key;
        self.image.do_blocking(move |image| id.name(&image).map(|s| s.into_owned())).await
    }
}

impl<'a> Snapshot<'a, CString> {
    /// Create a [`Snapshot`] instance identifying the snapshot through its name.
    ///
    /// Note that this does not check whether the snapshot exists. To do so you must call [`Snapshot::exists`].
    pub fn from_name<N: TryIntoCString>(image: &'a Image<'a>, name: N) -> Result<Self, N::Error> {
        Ok(Snapshot {
            image,
            key: name.try_into_cstring()?,
        })
    }

    /// Get the unique, immutable ID of the snapshot.
    ///
    /// Since this [`Snapshot`] object is identified by name, this must be queried.
    pub async fn id(&self) -> crate::Result<SnapId> {
        let id = self.key.clone();
        self.image.do_blocking(move |image| id.id(&image)).await
    }

    /// Get the name of this snapshot.
    pub fn name(&self) -> &CStr {
        &self.key
    }
}

impl<'a> Snapshot<'a, SnapKey> {
    /// Create a [`Snapshot`] instance identifying the snapshot through either its name or ID, to
    /// be determined at runtime.
    ///
    /// Note that this does not check whether the snapshot exists. To do so you must call [`Snapshot::exists`].
    pub fn from_key(image: &'a Image<'a>, key: SnapKey) -> Self {
        Snapshot { image, key }
    }

    /// Get the unique, immutable ID of the snapshot.
    pub async fn id(&self) -> crate::Result<SnapId> {
        match &self.key {
            SnapKey::Id(id) => Ok(*id),
            SnapKey::Name(name) => {
                let name = name.clone();
                self.image.do_blocking(move |image| name.id(&image)).await
            }
        }
    }

    /// Get the name of this snapshot.
    pub async fn name(&self) -> crate::Result<Cow<'_, CStr>> {
        match &self.key {
            SnapKey::Id(id) => {
                let id = *id;
                self.image
                    .do_blocking(move |image| id.name(&image).map(|s| s.into_owned()))
                    .await
                    .map(Cow::Owned)
            }
            SnapKey::Name(name) => Ok(Cow::Borrowed(name)),
        }
    }
}

impl<'a> From<Snapshot<'a, SnapId>> for Snapshot<'a> {
    fn from(value: Snapshot<'a, SnapId>) -> Self {
        Self {
            image: value.image,
            key: value.key.into(),
        }
    }
}

impl<'a> From<Snapshot<'a, CString>> for Snapshot<'a> {
    fn from(value: Snapshot<'a, CString>) -> Self {
        Self {
            image: value.image,
            key: value.key.into(),
        }
    }
}

impl<'a, Key: AsSnapKey> From<Snapshot<'a, Key>> for SnapKey {
    fn from(value: Snapshot<'a, Key>) -> Self {
        value.key.into()
    }
}

impl<'a, Key: AsSnapKey> From<&Snapshot<'a, Key>> for SnapKey {
    fn from(value: &Snapshot<'a, Key>) -> Self {
        value.as_snap_key().into_owned()
    }
}

/// Types of RBD namespaces that a snapshot can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SnapNamespaceType {
    /// The user namespace, containing image snapshots.
    User,
    /// The group namespace, containing snapshots of image groups.
    Group,
    /// The mirror namespace.
    Mirror,
    /// The trash namespace, containing snapshots of trashed images.
    Trash,
    /// Unknown (perhaps new?) namespace type.
    Other(u32),
}

impl From<rbd_snap_namespace_type_t> for SnapNamespaceType {
    fn from(value: rbd_snap_namespace_type_t) -> Self {
        match value {
            rbd_snap_namespace_type_t::RBD_SNAP_NAMESPACE_TYPE_USER => Self::User,
            rbd_snap_namespace_type_t::RBD_SNAP_NAMESPACE_TYPE_GROUP => Self::Group,
            rbd_snap_namespace_type_t::RBD_SNAP_NAMESPACE_TYPE_MIRROR => Self::Mirror,
            rbd_snap_namespace_type_t::RBD_SNAP_NAMESPACE_TYPE_TRASH => Self::Trash,
            rbd_snap_namespace_type_t(typ) => Self::Other(typ),
        }
    }
}

impl From<u32> for SnapNamespaceType {
    fn from(value: u32) -> Self {
        rbd_snap_namespace_type_t(value).into()
    }
}

impl From<SnapNamespaceType> for rbd_snap_namespace_type_t {
    fn from(value: SnapNamespaceType) -> Self {
        match value {
            SnapNamespaceType::User => rbd_snap_namespace_type_t::RBD_SNAP_NAMESPACE_TYPE_USER,
            SnapNamespaceType::Group => rbd_snap_namespace_type_t::RBD_SNAP_NAMESPACE_TYPE_GROUP,
            SnapNamespaceType::Mirror => rbd_snap_namespace_type_t::RBD_SNAP_NAMESPACE_TYPE_MIRROR,
            SnapNamespaceType::Trash => rbd_snap_namespace_type_t::RBD_SNAP_NAMESPACE_TYPE_TRASH,
            SnapNamespaceType::Other(typ) => rbd_snap_namespace_type_t(typ),
        }
    }
}

impl From<SnapNamespaceType> for u32 {
    fn from(value: SnapNamespaceType) -> Self {
        rbd_snap_namespace_type_t::from(value).0
    }
}

/// Basic identifying information about a RBD snapshot.
#[repr(transparent)]
pub struct SnapSpec {
    pub(crate) inner: rbd_sys::rbd_snap_spec_t,
}

unsafe impl Send for SnapSpec {}
unsafe impl Sync for SnapSpec {}

impl Drop for SnapSpec {
    fn drop(&mut self) {
        unsafe { rbd_sys::rbd_snap_spec_cleanup(&mut self.inner) };
    }
}

impl std::fmt::Debug for SnapSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapSpec")
            .field("id", &self.id())
            .field("name", &self.name())
            .field("namespace_type", &self.namespace_type())
            .finish()
    }
}

impl SnapSpec {
    /// The snapshot's unique, immutable 64-bit ID.
    pub fn id(&self) -> SnapId {
        SnapId(self.inner.id)
    }

    /// The snapshot's name.
    pub fn name(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.inner.name) }
    }

    /// The namespace type for this snapshot.
    pub fn namespace_type(&self) -> SnapNamespaceType {
        self.inner.namespace_type.into()
    }
}

/// Basic identifying information about a RBD snapshot.
#[repr(transparent)]
pub struct SnapInfo {
    inner: rbd_sys::rbd_snap_info_t,
}

unsafe impl Send for SnapInfo {}
unsafe impl Sync for SnapInfo {}

impl std::fmt::Debug for SnapInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapInfo")
            .field("id", &self.id())
            .field("size", &self.size())
            .field("name", &self.name())
            .finish()
    }
}

/// Basic information about a RBD snapshot.
impl SnapInfo {
    /// The snapshot's unique, immutable 64-bit ID.
    pub fn id(&self) -> SnapId {
        SnapId(self.inner.id)
    }

    /// The snapshot's size.
    pub fn size(&self) -> u64 {
        self.inner.size
    }

    /// The snapshot's name.
    pub fn name(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.inner.name) }
    }
}

/// List of image snapshot information obtained from [`Image::snap_list`](super::Image::snap_list).
///
/// The list of [`SnapInfo`]s is exposed via a [`Deref`] implementation.
pub struct SnapInfoList(Vec<SnapInfo>);

impl Drop for SnapInfoList {
    fn drop(&mut self) {
        unsafe { rbd_sys::rbd_snap_list_end(self.0.as_mut_ptr().cast()) };
    }
}

impl AsRef<[SnapInfo]> for SnapInfoList {
    fn as_ref(&self) -> &[SnapInfo] {
        // The last snapshot is a null-terminator without any information.
        &self.0[.. self.0.len() - 1]
    }
}

impl Deref for SnapInfoList {
    type Target = [SnapInfo];

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

// We need to manually impl Debug to bypass the Vec's impl, since the last
// SnapInfo has a null name ptr.
impl std::fmt::Debug for SnapInfoList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&AsRef::<[SnapInfo]>::as_ref(self), f)
    }
}

impl metadata::Sealed for SnapInfoList {
    unsafe fn get(image: rbd_sys::rbd_image_t) -> crate::Result<Self> {
        unsafe {
            let snaps = util::get_known_size_arr(
                |buf: *mut SnapInfo, size| {
                    let mut max_snaps = (*size).min(i32::MAX as usize) as i32;
                    let result = rbd_sys::rbd_snap_list(image, buf.cast(), &mut max_snaps);
                    *size = max_snaps as usize;
                    // rbs_snap_list doesn't write to `max_snaps` when successful, for some reason
                    // this is not the case for other functions like this
                    if result >= 0 {
                        *size = result as usize + 1;
                    }
                    result
                },
                8,
            )?;
            Ok(Self(snaps))
        }
    }
}
impl metadata::ImageMeta for SnapInfoList {}

bitflags::bitflags! {
    /// Flags that can be passed to [`Image::snap_create`](super::Image::snap_create).
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct SnapCreateFlags: u32 {
        const NONE = 0;
        /// Don't quiesce while creating the snapshot.
        ///
        /// The image is typically quiesced (all write I/O is blocked) during snapshot creation
        /// to ensure that all clients can rely on the snapshot being a consistent save point.
        ///
        /// By using this flag, you are responsible for ensuring the consistency of the snapshot.
        const SKIP_QUIESCE = 1;
        /// Ignore any quiesce errors instead of cancelling snapshot creation and reporting an error.
        ///
        /// Note that this may lead to an inconsistent snapshot.
        const IGNORE_QUIESCE_ERROR = 2;
    }
}

bitflags::bitflags! {
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    /// Flags that can be passed to [`Image::snap_remove`](super::Image::snap_remove).
    pub struct SnapRemoveFlags: u32 {
        const NONE = 0;
        /// If the snapshot is protected, unprotect it before removal.
        const UNPROTECT = 1;
        /// Flattens child images (those created through [`IoCtx::Clone`](crate::rados::IoCtx::rbd_clone))
        /// of the snapshot being deleted.
        ///
        /// This must be done before removing a snapshot, unless the deep flatten feature was enabled.
        const FLATTEN = 2;
        /// Unprotect the snapshot and flattens child images, effectively forcing its removal.
        const FORCE = Self::UNPROTECT.bits() | Self::FLATTEN.bits();
    }
}

/// Snapshot limit for an image.
///
/// Implements [`ImageMeta`](super::metadata::ImageMeta) and can thus be queried using
/// [`Image::get_metadata`](super::Image::get_metadata).
pub struct SnapLimit(pub u64);

impl metadata::Sealed for SnapLimit {
    unsafe fn get(image: rbd_sys::rbd_image_t) -> crate::Result<Self> {
        let mut limit = u64::MAX;
        util::check_os_error(unsafe { rbd_sys::rbd_snap_get_limit(image, &mut limit) })?;
        Ok(Self(limit))
    }
}
impl metadata::ImageMeta for SnapLimit {}

/// Information about a snapshot's namespace.
#[derive(Debug)]
#[non_exhaustive]
pub enum SnapNamespace {
    /// User namespace information.
    User,
    /// Group namespace information.
    Group(SnapGroupNamespace),
    /// Trash namespace information.
    #[cfg(feature = "rbd_v1-20")]
    Trash(SnapTrashNamespace),
    /// Mirror namespace information.
    Mirror(SnapMirrorNamespace),
    /// Namespace type for which fetching information is not supported.
    Unsupported(SnapNamespaceType),
}

impl SnapNamespace {
    unsafe fn new(image: rbd_sys::rbd_image_t, id: SnapId) -> crate::Result<Self> {
        let mut ns_type = rbd_sys::rbd_snap_namespace_type_t::RBD_SNAP_NAMESPACE_TYPE_USER;
        check_os_error(unsafe { rbd_sys::rbd_snap_get_namespace_type(image, id.0, &mut ns_type) })?;
        let ns_type: SnapNamespaceType = ns_type.into();

        type NsGetter<T> = unsafe extern "C" fn(rbd_sys::rbd_image_t, u64, *mut T, usize) -> c_int;
        unsafe fn get<T>(image: rbd_sys::rbd_image_t, id: SnapId, f: NsGetter<T>) -> crate::Result<T> {
            let mut ns = MaybeUninit::uninit();
            check_os_error(unsafe { f(image, id.0, ns.as_mut_ptr(), size_of::<T>()) })?;
            Ok(unsafe { ns.assume_init() })
        }

        unsafe {
            Ok(match ns_type {
                SnapNamespaceType::User => SnapNamespace::User,
                SnapNamespaceType::Group => SnapNamespace::Group(SnapGroupNamespace {
                    inner: get(image, id, rbd_sys::rbd_snap_get_group_namespace)?,
                }),
                #[cfg(feature = "rbd_v1-20")]
                SnapNamespaceType::Trash => SnapNamespace::Trash(SnapTrashNamespace {
                    inner: get(image, id, rbd_sys::rbd_snap_get_trash_namespace2)?,
                }),
                #[cfg(not(feature = "rbd_v1-20"))]
                SnapNamespaceType::Trash => SnapNamespace::Unsupported(SnapNamespaceType::Trash),
                SnapNamespaceType::Mirror =>
                    SnapNamespace::Mirror(SnapMirrorNamespace::new(get(image, id, rbd_sys::rbd_snap_get_mirror_namespace)?)),
                other => SnapNamespace::Unsupported(other),
            })
        }
    }

    /// Get the type of this snapshot namespace.
    pub fn namespace_type(&self) -> SnapNamespaceType {
        match self {
            Self::User => SnapNamespaceType::User,
            Self::Group(_) => SnapNamespaceType::Group,
            #[cfg(feature = "rbd_v1-20")]
            Self::Trash(_) => SnapNamespaceType::Trash,
            Self::Mirror(_) => SnapNamespaceType::Mirror,
            Self::Unsupported(typ) => *typ,
        }
    }
}

/// Information about a snapshot in the group namespace.
#[repr(transparent)]
pub struct SnapGroupNamespace {
    inner: rbd_sys::rbd_snap_group_namespace_t,
}

unsafe impl Send for SnapGroupNamespace {}
unsafe impl Sync for SnapGroupNamespace {}

impl SnapGroupNamespace {
    /// The ID of the RBD pool this snapshot group is stored in.
    pub fn group_pool(&self) -> u64 {
        self.inner.group_pool as u64
    }

    /// The group's name.
    pub fn group_name(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.inner.group_name) }
    }

    /// The name of the group snapshot, or [`None`] if part of the group's writable head.
    pub fn group_snap_name(&self) -> Option<&CStr> {
        if self.inner.group_snap_name.is_null() {
            None
        } else {
            Some(unsafe { CStr::from_ptr(self.inner.group_snap_name) })
        }
    }
}

impl std::fmt::Debug for SnapGroupNamespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapGroupNamespace")
            .field("group_pool", &self.group_pool())
            .field("group_name", &self.group_name())
            .field("group_snap_name", &self.group_snap_name())
            .finish()
    }
}

impl Drop for SnapGroupNamespace {
    fn drop(&mut self) {
        unsafe { rbd_sys::rbd_snap_group_namespace_cleanup(&mut self.inner, size_of_val(&self.inner)) };
    }
}

#[cfg(feature = "rbd_v1-20")]
mod snap_trash_namespace {
    use super::*;

    /// Information about a snapshot in the trash namespace.
    #[repr(transparent)]
    pub struct SnapTrashNamespace {
        pub(super) inner: rbd_sys::rbd_snap_trash_namespace_t,
    }

    unsafe impl Send for SnapTrashNamespace {}
    unsafe impl Sync for SnapTrashNamespace {}

    impl SnapTrashNamespace {
        /// The original namespace type of the snapshot before it was moved to the trash.
        pub fn original_namespace_type(&self) -> SnapNamespaceType {
            self.inner.original_namespace_type.into()
        }

        /// The original name of the snapshot before it was moved to the trash.
        pub fn original_name(&self) -> &CStr {
            unsafe { CStr::from_ptr(self.inner.original_name) }
        }
    }

    impl std::fmt::Debug for SnapTrashNamespace {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SnapTrashNamespace")
                .field("original_namespace_type", &self.original_namespace_type())
                .field("original_name", &self.original_name())
                .finish()
        }
    }

    impl Drop for SnapTrashNamespace {
        fn drop(&mut self) {
            unsafe { rbd_sys::rbd_snap_trash_namespace_cleanup(&mut self.inner, size_of_val(&self.inner)) };
        }
    }
}

#[cfg(feature = "rbd_v1-20")]
#[doc(inline)]
pub use snap_trash_namespace::*;

/// The state of a RBD mirror snapshot.
#[repr(u32)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum RbdSnapMirrorState {
    Primary,
    Demoted,
    NonPrimary,
    NonPrimaryDemoted,
    Other(u32),
}

impl From<rbd_sys::rbd_snap_mirror_state_t> for RbdSnapMirrorState {
    fn from(value: rbd_sys::rbd_snap_mirror_state_t) -> Self {
        match value {
            rbd_sys::rbd_snap_mirror_state_t::RBD_SNAP_MIRROR_STATE_PRIMARY => Self::Primary,
            rbd_sys::rbd_snap_mirror_state_t::RBD_SNAP_MIRROR_STATE_PRIMARY_DEMOTED => Self::Demoted,
            rbd_sys::rbd_snap_mirror_state_t::RBD_SNAP_MIRROR_STATE_NON_PRIMARY => Self::NonPrimary,
            rbd_sys::rbd_snap_mirror_state_t::RBD_SNAP_MIRROR_STATE_NON_PRIMARY_DEMOTED => Self::NonPrimaryDemoted,
            unk => Self::Other(unk.0),
        }
    }
}

/// Information about a snapshot in the mirror namespace.
pub struct SnapMirrorNamespace {
    inner: rbd_sys::rbd_snap_mirror_namespace_t,
    // split the mirror_peer_uuids eagerly
    // note: the CStrs are owned by this type; it would be unsound to expose them as 'static!
    // ideally we would use a `*const [c_char]` and transmute, but the layout of CStr is not stable
    // This *should* be sound even though the references will be dangling on Drop
    peer_uuids: Vec<&'static CStr>,
}

unsafe impl Send for SnapMirrorNamespace {}
unsafe impl Sync for SnapMirrorNamespace {}

impl SnapMirrorNamespace {
    /// # Safety
    ///
    /// `inner` must be a valid rbd_snap_mirror_namespace_t instance.
    pub(super) unsafe fn new(inner: rbd_sys::rbd_snap_mirror_namespace_t) -> Self {
        // the peer_uuids are nul-terminated strings stored one after the other in a contiguous buffer.
        // extract pointers to them and store them separately.
        let mut p = inner.mirror_peer_uuids;
        let mut peer_uuids = Vec::with_capacity(inner.mirror_peer_uuids_count);
        for _ in 0 .. inner.mirror_peer_uuids_count {
            let cstr = unsafe { CStr::from_ptr(p) };
            p = unsafe { p.add(cstr.count_bytes() + 1) };
            peer_uuids.push(cstr);
        }
        Self { inner, peer_uuids }
    }

    /// The snapshot's mirror state.
    pub fn state(&self) -> RbdSnapMirrorState {
        self.inner.state.into()
    }

    /// The UUIDs of the snapshot's RBD mirror peers.
    pub fn mirror_peer_uuids(&self) -> &[&CStr] {
        self.peer_uuids.as_slice()
    }

    /// The UUID of the primary mirror.
    pub fn primary_mirror_uuid(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.inner.primary_mirror_uuid) }
    }

    /// `true` if all replicas have finished synchronizing, and `false` if a mirroring
    /// operation is still in progress.
    pub fn complete(&self) -> bool {
        self.inner.complete
    }

    /// The snapshot ID of the primary replica.
    pub fn primary_snap_id(&self) -> SnapId {
        SnapId(self.inner.primary_snap_id)
    }

    /// ID of the last RADOS object that was copied as part of the mirroring process.
    pub fn last_copied_object_number(&self) -> u64 {
        self.inner.last_copied_object_number
    }
}

impl std::fmt::Debug for SnapMirrorNamespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapMirrorNamespace")
            .field("state", &self.state())
            .field("mirror_peer_uuids", &self.mirror_peer_uuids())
            .field("complete", &self.complete())
            .field("primary_mirror_uuid", &self.primary_mirror_uuid())
            .field("primary_snap_id", &self.primary_snap_id())
            .field("last_copied_object_number", &self.last_copied_object_number())
            .finish()
    }
}

impl Drop for SnapMirrorNamespace {
    fn drop(&mut self) {
        unsafe { rbd_sys::rbd_snap_mirror_namespace_cleanup(&mut self.inner, size_of_val(&self.inner)) };
    }
}
