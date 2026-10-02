// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bindings for the librbd advisory locking API.

use std::{
    ffi::{CStr, CString, c_int},
    io::ErrorKind,
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    str::Utf8Error,
};

use crate::{
    Error,
    Result,
    async_rt::{DynExecutor, Executor},
    rbd::{Image, ImageHandle},
    util::{TryIntoCString, check_os_error},
};

use std::result::Result as StdResult;

/// A (client, cookie) pair that uniquely identifies a lock holder.
#[derive(Debug, Clone)]
pub struct Locker {
    /// The ceph client identifier.
    pub client: String,
    /// The cookie set by the client when acquiring the lock.
    pub cookie: CString,
}

/// A librbd client that holds an advisory lock on an image.
#[derive(Debug, Clone)]
pub struct LockerInfo {
    /// (client, cookie) pair that uniquely identifies the lock holder.
    locker: Locker,
    /// The locker's IP address.
    pub address: String,
}

impl From<LockerInfo> for Locker {
    fn from(value: LockerInfo) -> Self {
        value.locker
    }
}

impl AsRef<Locker> for LockerInfo {
    fn as_ref(&self) -> &Locker {
        &self.locker
    }
}

impl AsMut<Locker> for LockerInfo {
    fn as_mut(&mut self) -> &mut Locker {
        &mut self.locker
    }
}

// implement Deref[Mut] for ergonomics:
// - direct access to client and cookie fields;
// - so that a `&LockerInfo` can be transparently passed to `break_lock`.

impl Deref for LockerInfo {
    type Target = Locker;

    fn deref(&self) -> &Self::Target {
        &self.locker
    }
}

impl DerefMut for LockerInfo {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.locker
    }
}

/// Information about an advisory lock held on an image.
#[derive(Debug, Clone)]
pub enum LockInfo {
    /// Exclusive lock, held by a single client.
    Exclusive(LockerInfo),
    /// Shared lock, held by at least one client.
    Shared {
        /// The tag the shared lock was acquired with.
        tag: CString,
        /// List of lockers that hold the lock.
        lockers: Vec<LockerInfo>,
    },
}

impl LockInfo {
    /// Safe blocking wrapper around `rbd_list_lockers`.
    pub(crate) fn get(img: &ImageHandle) -> crate::Result<Option<Self>> {
        let mut is_exclusive: c_int = 0;

        // Note: we can't use the ffi utils here, because of the amount of buffers we have to manage
        let [mut tag, mut clients, mut cookies, mut addrs] = [(); 4].map(|_| {
            let vec = Vec::<u8>::with_capacity(128);
            let cap = vec.capacity();
            (vec, cap)
        });

        let num_lockers = loop {
            let result = unsafe {
                rbd_sys::rbd_list_lockers(
                    img.as_ptr(),
                    &mut is_exclusive,
                    tag.0.as_mut_ptr().cast(),
                    &mut tag.1,
                    clients.0.as_mut_ptr().cast(),
                    &mut clients.1,
                    cookies.0.as_mut_ptr().cast(),
                    &mut cookies.1,
                    addrs.0.as_mut_ptr().cast(),
                    &mut addrs.1,
                )
            };

            const ERANGE: isize = -libc::ERANGE as isize;
            match result {
                0 => return Ok(None),
                num_lockers if num_lockers > 0 => break num_lockers as usize,
                // reserve the required space on all buffers
                ERANGE =>
                    for (buf, size) in [&mut tag, &mut clients, &mut cookies, &mut addrs] {
                        buf.reserve(*size)
                    },
                e => return Err(std::io::Error::from_raw_os_error(-e as i32).into()),
            }
        };

        // set lengths on all buffers
        let [tag, clients, cookies, addrs] = [tag, clients, cookies, addrs].map(|(mut buf, size)| {
            // SAFETY: by the contract of rbd_list_watchers
            unsafe { buf.set_len(size) };
            buf
        });

        // split the clients, cookies and addrs buffers
        let [clients, cookies, addrs] = [&clients, &cookies, &addrs].map(|buf| buf.split_inclusive(|&c| c == 0));

        let mut lockers = clients
            .zip(cookies)
            .zip(addrs)
            // SAFETY: the presence of the nul terminator is guaranteed by split_inclusive and the contract
            // of rbd_list_watchers, which guarantees the buffers are null-terminated.
            .map(|((client, cookie), addr)| unsafe {
                Ok::<_, Utf8Error>(LockerInfo {
                    locker: Locker {
                        client: CStr::from_bytes_with_nul_unchecked(client).to_str()?.to_owned(),
                        cookie: CStr::from_bytes_with_nul_unchecked(cookie).to_owned(),
                    },
                    address: CStr::from_bytes_with_nul_unchecked(addr).to_str()?.to_owned(),
                })
            })
            .map(|r| r.map_err(|_| Error::unexpected("invalid utf8 in rados client/address name")));

        if is_exclusive == 1 {
            if num_lockers != 1 {
                return Err(Error::unexpected("rbd_list_lockers returned exclusive lock with more than 1 owners"));
            }
            let locker = lockers
                .next()
                // This is upheld by rbd_list_watchers's contract and should not error outside of librbd bugs
                .ok_or(Error::unexpected("rbd_list_lockers reported one locker, but populated zero"))??;

            Ok(Some(Self::Exclusive(locker)))
        } else {
            // SAFETY: the tag is nul-terminated by the contract of rbd_list_watchers.
            let tag = unsafe { CString::from_vec_with_nul_unchecked(tag) };
            let lockers = lockers.collect::<Result<Vec<_>>>()?;

            // this will never trigger modulo librbd bugs, but may be helpful to catch them
            if lockers.len() != num_lockers {
                return Err(Error::unexpected("rbd_list_lockers reported incorrect number of lockers"));
            }

            Ok(Some(Self::Shared { tag, lockers }))
        }
    }

    /// Whether this lock is an exclusive lock.
    pub const fn is_exclusive(&self) -> bool {
        matches!(self, Self::Exclusive(_))
    }

    /// Whether this lock is a shared lock.
    pub const fn is_shared(&self) -> bool {
        matches!(self, Self::Shared { .. })
    }

    /// Get a slice of all lockers. It will always contain at least one element.
    pub const fn lockers(&self) -> &[LockerInfo] {
        match self {
            Self::Exclusive(l) => std::slice::from_ref(l),
            Self::Shared { lockers, .. } => lockers.as_slice(),
        }
    }

    /// Get a mutable slice of all lockers. It will always contain at least one element.
    pub const fn lockers_mut(&mut self) -> &mut [LockerInfo] {
        match self {
            Self::Exclusive(l) => std::slice::from_mut(l),
            Self::Shared { lockers, .. } => lockers.as_mut_slice(),
        }
    }
}

impl AsRef<[LockerInfo]> for LockInfo {
    fn as_ref(&self) -> &[LockerInfo] {
        self.lockers()
    }
}

impl AsMut<[LockerInfo]> for LockInfo {
    fn as_mut(&mut self) -> &mut [LockerInfo] {
        self.lockers_mut()
    }
}

/// Extension type that implements common methods of [`LockInfo`]
/// on [`Option<LockInfo>`], where it makes sense.
///
/// For example, this allows transparently calling `.lockers()` directly on
/// the output of [`AdvisoryLock::info`].
pub trait LockerInfoExt {
    /// Get a slice of all lockers. It is empty if no lock is held.
    fn lockers(&self) -> &[LockerInfo];

    /// Get a mutable slice of all lockers. It is empty if no lock is held.
    fn lockers_mut(&mut self) -> &mut [LockerInfo];
}

impl LockerInfoExt for Option<LockInfo> {
    fn lockers(&self) -> &[LockerInfo] {
        self.as_ref().map_or(&[], |lock| lock.lockers())
    }

    fn lockers_mut(&mut self) -> &mut [LockerInfo] {
        self.as_mut().map_or(&mut [], |lock| lock.lockers_mut())
    }
}

/// Wrapper around an [`Image`] reference that serves as an access point and builder for advisory locking APIs.
///
/// Advisory locks do not synchronize any operations on the image besides acquiring other advisory locks.
#[derive(Debug)]
pub struct AdvisoryLock<'a, 'b> {
    image: &'b Image<'a>,
    cookie: Option<CString>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum LockError {
    /// The lock is currently held by another [`Locker`].
    #[error("Lock is currently held by another locker")]
    HeldByOther,
    /// The [`Locker`] that requested the lock already holds it.
    #[error("Lock is is already held by self")]
    HeldBySelf,
}

pub type LockResult<'a, 'b> = StdResult<LockGuard<'a, 'b>, LockError>;
pub type SharedLockResult<'a, 'b> = StdResult<SharedLockGuard<'a, 'b>, LockError>;

impl<'a, 'b> AdvisoryLock<'a, 'b> {
    pub(super) fn new(image: &'b Image<'a>) -> Self {
        Self { image, cookie: None }
    }

    /// Get information about the currently held advisory lock, including a list of all clients holding the lock.
    ///
    /// Returns [`None`] if no lock is currently held.
    #[tracing::instrument("AdvisoryLock::info", level = "debug", err)]
    pub async fn info(self) -> Result<Option<LockInfo>> {
        self.image.do_blocking(|image| LockInfo::get(&image)).await
    }

    /// Manually specify a cookie to use for acquiring a shared/exclusive lock.
    ///
    /// By default, a UUID7 based on both the system time and random bits is used to guarantee
    /// uniqueness of the cookie.
    pub fn cookie<C: TryIntoCString>(self, cookie: C) -> StdResult<Self, C::Error> {
        let cookie = Some(cookie.try_into_cstring()?);
        Ok(Self { cookie, ..self })
    }

    fn uuid_cookie() -> CString {
        let ts = uuid::Timestamp::now(uuid::NoContext);
        // Panic safety: the uuid string representation has no nul bytes
        CString::new(uuid::Uuid::new_v7(ts).to_string()).unwrap()
    }

    /// Try to acquire an exclusive advisory lock on this image.
    ///
    /// If a lock cookie is not manually specified with [`AdvisoryLock::cookie`], a UUID7 based on
    /// both the system time and random bits is used. This should be sufficient for virtually all situations.
    #[tracing::instrument("AdvisoryLock::acquire", level = "debug", err)]
    pub async fn acquire(self) -> Result<LockResult<'a, 'b>> {
        let cookie = self.cookie.unwrap_or_else(Self::uuid_cookie);

        let owned_lock = self
            .image
            .do_blocking(move |image| match unsafe { -rbd_sys::rbd_lock_exclusive(image.as_ptr(), cookie.as_ptr()) } {
                libc::EBUSY => Ok(Err(LockError::HeldByOther)),
                libc::EEXIST => Ok(Err(LockError::HeldBySelf)),
                e if e > 0 => Err(Error::from(std::io::Error::from_raw_os_error(e))),
                _ => Ok(Ok(BlockingLockGuard { image, cookie })),
            })
            .await?;

        Ok(owned_lock.map(move |lock| LockGuard {
            executor: self.image.executor,
            inner: lock,
            _phantom: PhantomDrop::new(),
        }))
    }

    /// Try to acquire a shared advisory lock on this image with the provided tag.
    ///
    /// Other clients who wish to acquire the shared lock must use the same tag.
    ///
    /// If a lock cookie is not manually specified with [`AdvisoryLock::cookie`], a UUID7 based on
    /// both the system time and random bits is used. This should be sufficient for virtually all situations.
    #[tracing::instrument("AdvisoryLock::acquire_shared", level = "debug", skip(tag), err)]
    pub async fn acquire_shared(self, tag: impl TryIntoCString) -> Result<SharedLockResult<'a, 'b>> {
        let tag = tag.try_into_cstring().map_err(Error::ffi)?;
        let cookie = self.cookie.unwrap_or_else(Self::uuid_cookie);

        let owned_lock = self
            .image
            .do_blocking(move |image| match unsafe { -rbd_sys::rbd_lock_shared(image.as_ptr(), cookie.as_ptr(), tag.as_ptr()) } {
                libc::EBUSY => Ok(Err(LockError::HeldByOther)),
                libc::EEXIST => Ok(Err(LockError::HeldBySelf)),
                e if e > 0 => Err(Error::from(std::io::Error::from_raw_os_error(e))),
                _ => Ok(Ok((BlockingLockGuard { image, cookie }, tag))),
            })
            .await?;

        Ok(owned_lock.map(move |(lock, tag)| SharedLockGuard {
            guard: LockGuard {
                executor: self.image.executor,
                inner: lock,
                _phantom: PhantomDrop::new(),
            },
            tag,
        }))
    }

    /// Force the release of a shared or exclusive lock that was taken by the specified [`Locker`].
    ///
    /// This identifier is provided by the [`AdvisoryLock::info`] method.
    ///
    /// On success, returns `true` if the lock was broken and `false` if the locker was not holding the lock.
    ///
    /// # Warning
    ///
    /// To avoid compromising the integrity of the locks, this instructs the Ceph monitors
    /// to blacklist `locker.client`. As such, calling this on one of your own locks will
    /// lead to every future librados/librbd call, including the current one, to fail!
    #[tracing::instrument("AdvisoryLock::break_lock", level = "debug", err)]
    pub async fn break_lock(self, locker: &Locker) -> Result<bool> {
        let client = locker.client.as_str().try_into_cstring().map_err(Error::ffi)?;
        let cookie = locker.cookie.clone();
        let result = self
            .image
            .do_blocking(move |image| unsafe { rbd_sys::rbd_break_lock(image.as_ptr(), client.as_ptr(), cookie.as_ptr()) })
            .await;

        match check_os_error(result) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Debug)]
struct BlockingLockGuard {
    image: ImageHandle,
    cookie: CString,
}

impl BlockingLockGuard {
    /// Like [`BlockingLockGuard::drop`], but doesn't hide failures from the caller.
    fn unlock(self) -> Result<()> {
        // Since we implement `Drop`, manual destructuring is required
        let this = ManuallyDrop::new(self);
        // SAFETY: the values are read from a ManuallyDrop, so they will not be dropped twice
        let image = unsafe { (&raw const this.image).read() };
        let cookie = unsafe { (&raw const this.cookie).read() };

        check_os_error(unsafe { rbd_sys::rbd_unlock(image.as_ptr(), cookie.as_ptr()) })?;
        Ok(())
    }
}

impl Drop for BlockingLockGuard {
    fn drop(&mut self) {
        if let Err(e) = check_os_error(unsafe { rbd_sys::rbd_unlock(self.image.as_ptr(), self.cookie.as_ptr()) }) {
            tracing::error!("failed to unlock BlockingLockGuard during drop: {e}");
        }
    }
}

/// PhantomData, but implements [`Drop`] manually. This can be used to force drop checking
/// over a particular generic parameter without implementing [`Drop`] directly.
///
/// See https://doc.rust-lang.org/nomicon/dropck.html for more information.
struct PhantomDrop<T: ?Sized>(PhantomData<T>);

impl<T: ?Sized> PhantomDrop<T> {
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T: ?Sized> Drop for PhantomDrop<T> {
    fn drop(&mut self) {}
}

impl<T: ?Sized> core::fmt::Debug for PhantomDrop<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

/// Exclusive advisory lock guard object.
///
/// Releases the lock when dropped. However, since
/// [`AsyncDrop`](https://doc.rust-lang.org/std/future/trait.AsyncDrop.html)
/// is unstable, this is a blocking operation. Consider using [`LockGuard::unlock`]
/// to avoid blocking the async runtime.
#[derive(Debug)]
pub struct LockGuard<'a, 'b> {
    executor: &'a dyn DynExecutor,
    inner: BlockingLockGuard,
    _phantom: PhantomDrop<&'b Image<'a>>,
}

impl<'a> LockGuard<'a, '_> {
    /// Get the cookie that uniquely represents this [`LockGuard`].
    pub fn cookie(&self) -> &CStr {
        &self.inner.cookie
    }

    /// Detach this [`LockGuard`] from the borrow it holds on its associated [`Image`].
    ///
    /// This allows it to be held past the lifetime of the [`Image`] object, at the risk
    /// of causing deadlocks if held past [`Image::close`].
    pub fn into_owned(self) -> LockGuard<'a, 'a> {
        LockGuard {
            executor: self.executor,
            inner: self.inner,
            _phantom: PhantomDrop::new(),
        }
    }

    /// Asynchronously release the lock.
    pub async fn unlock(self) -> Result<()> {
        let guard = self.inner;
        self.executor
            .spawn_blocking(move || guard.unlock())
            .await
            .expect("blocking task panicked")
    }
}

/// Shared advisory lock guard object.
///
/// Releases the lock when dropped. However, since
/// [`AsyncDrop`](https://doc.rust-lang.org/std/future/trait.AsyncDrop.html)
/// is unstable, this is a blocking operation. Consider using [`SharedLockGuard::unlock`]
/// to avoid blocking the async runtime.
pub struct SharedLockGuard<'a, 'b> {
    guard: LockGuard<'a, 'b>,
    tag: CString,
}

impl<'a> SharedLockGuard<'a, '_> {
    /// Get the cookie that uniquely represents this [`LockGuard`].
    pub fn cookie(&self) -> &CStr {
        self.guard.cookie()
    }

    /// Detach this [`SharedLockGuard`] from the borrow it holds on its associated [`Image`].
    ///
    /// This allows it to be held past the lifetime of the [`Image`] object, at the risk
    /// of causing deadlocks if held past [`Image::close`].
    pub fn into_owned(self) -> SharedLockGuard<'a, 'a> {
        SharedLockGuard {
            guard: self.guard.into_owned(),
            tag: self.tag,
        }
    }

    /// Get the tag associated with this shared advisory lock.
    ///
    /// Other shared lockers must provide the same tag to successfully acquire the lock.
    pub fn tag(&self) -> &CStr {
        &self.tag
    }

    /// Asynchronously release the lock.
    pub async fn unlock(self) -> Result<()> {
        self.guard.unlock().await
    }
}
