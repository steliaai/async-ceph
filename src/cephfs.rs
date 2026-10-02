// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{ffi::CString, fmt::Debug, path::Path, ptr::NonNull};

use cephfs_sys::{ceph_create_from_rados, ceph_init, ceph_mkdir, ceph_mount, ceph_select_filesystem, ceph_shutdown};
use tracing::Span;

use crate::{
    async_rt::{DynExecutor, Executor},
    rados::{RadosClient, RadosClientHandle},
    util::{ArcWait, check_os_error},
};

mod statx;
mod xattr;

pub use statx::Statx;
pub use xattr::SetXattrFlags;

/// Returns the current runtime version of libcephfs.
///
/// Note that this is unrelated to the Ceph version or the `async-ceph` version.
///
/// ```no_run
/// let cephfs_version = async_ceph::cephfs::version();
/// println!("CephFS version: {cephfs_version}");
/// ```
pub fn version() -> crate::LibVersion {
    let (mut major, mut minor, mut patch) = (0, 0, 0);
    unsafe { cephfs_sys::ceph_version(&mut major, &mut minor, &mut patch) };
    crate::LibVersion {
        major: major as u32,
        minor: minor as u32,
        patch: patch as u32,
    }
}

#[derive(Debug)]
pub(crate) struct CephfsPtr {
    #[allow(unused)]
    cluster: RadosClientHandle,
    mount: NonNull<cephfs_sys::ceph_mount_info>,
}

unsafe impl Send for CephfsPtr {}
unsafe impl Sync for CephfsPtr {}

impl CephfsPtr {
    pub fn as_ptr(&self) -> *mut cephfs_sys::ceph_mount_info {
        self.mount.as_ptr()
    }
}

impl Drop for CephfsPtr {
    /// Officially `ceph_shutdown` is deprecated and you're supposed to call
    /// `ceph_unmount` followed by `ceph_release`, which provide error handling.
    ///
    /// However, error handling is not really feasible in a Drop impl, and
    /// looking at the code shows that it's not really needed (at least at the moment):
    /// `ceph_unmount` returns an error if no filesystem is mounted, and
    /// otherwise unmounts it. `ceph_release` returns an error if a filesystem
    /// *is* mounted, and calls the `ceph_shutdown` code path otherwise.
    /// Meanwhile, `ceph_shutdown` simply unmounts the filesystem if it is mounted,
    /// so it's fine in any case.
    #[tracing::instrument("CephfsPtr::drop", level = "debug")]
    fn drop(&mut self) {
        unsafe { ceph_shutdown(self.as_ptr()) };
    }
}

/// Refcounted owned handle to a [`CephfsPtr`].
pub(crate) type CephfsHandle = ArcWait<CephfsPtr>;

/// Safe wrapper around a libcephfs [`ceph_mount_info`](cephfs_sys::ceph_mount_info).
pub struct CephfsMount<'a> {
    pub(crate) handle: CephfsHandle,
    pub(crate) executor: &'a dyn DynExecutor,
}

unsafe impl Send for CephfsMount<'_> {}
unsafe impl Sync for CephfsMount<'_> {}

impl Debug for CephfsMount<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CephfsMount").field("handle", &self.handle.as_ptr()).finish()
    }
}

impl<'a> CephfsMount<'a> {
    async fn do_op<F, R>(&self, span: Span, f: F) -> R
    where
        F: FnOnce(*mut cephfs_sys::ceph_mount_info) -> R + Send + 'static,
        R: Send + 'static, {
        let handle = self.handle.clone();

        let handle = self.executor.spawn_blocking(move || {
            let _enter = span.enter();
            f(handle.as_ptr())
        });

        handle.await.expect("failed to await blocking task")
    }

    /// Create a directory at the given path. Equivalent to POSIX' `mkdir(2)`.
    pub async fn mkdir(&self, path: impl AsRef<Path>, mode: u32) -> crate::Result<()> {
        let span = tracing::debug_span!("CephfsMount::mkdir");
        let path = CString::new(path.as_ref().as_os_str().as_encoded_bytes())?;

        self.do_op(span, move |mount| unsafe {
            check_os_error(ceph_mkdir(mount, path.as_ptr(), mode))?;
            Ok(())
        })
        .await
    }
}

/// Builder struct for mounting a CephFs and creating a [`CephfsMount`] instance.
#[derive(Debug)]
pub struct CephfsMountBuilder<'a> {
    mount: CephfsPtr,
    executor: &'a dyn DynExecutor,
    root: Option<CString>,
}

impl<'a> CephfsMountBuilder<'a> {
    /// Create a [`CephfsMountBuilder`] from the given [`RadosClient`].
    pub fn new(cluster: &RadosClient<'a>) -> crate::Result<Self> {
        let executor = cluster.get_executor();
        let cluster = cluster.get_handle();
        let mut mount = std::ptr::null_mut();
        unsafe { check_os_error(ceph_create_from_rados(&mut mount, cluster.as_ptr()))? };
        let mount = NonNull::new(mount).ok_or_else(|| crate::Error::unexpected("unexpected ceph_create_from_rados failure"))?;
        Ok(Self {
            mount: CephfsPtr { cluster, mount },
            executor,
            root: None,
        })
    }

    /// Specify the name of the file system to be mounted.
    /// This is not needed if there is only one file system.
    pub fn filesystem(self, filesystem: impl AsRef<str>) -> crate::Result<Self> {
        let filesystem = CString::new(filesystem.as_ref())?;
        unsafe { check_os_error(ceph_select_filesystem(self.mount.as_ptr(), filesystem.as_ptr()))? };
        Ok(self)
    }

    /// Specify a subpath in the file system that should be mounted.
    /// By default, the root directory is mounted.
    pub fn mount_root(mut self, root: impl AsRef<Path>) -> crate::Result<Self> {
        self.root = Some(CString::new(root.as_ref().as_os_str().as_encoded_bytes())?);
        Ok(self)
    }

    /// Mount the filesystem.
    pub async fn mount(self) -> crate::Result<CephfsMount<'a>> {
        let handle = CephfsHandle::new(self.mount);

        let (res, handle) = self
            .executor
            .spawn_blocking(move || unsafe { (check_os_error(ceph_init(handle.as_ptr())), handle) })
            .await
            .expect("failed to await init task");
        res?;

        let (res, handle) = self
            .executor
            .spawn_blocking(move || unsafe {
                let root = self.root.map_or(core::ptr::null(), |r| r.as_ptr());
                (check_os_error(ceph_mount(handle.as_ptr(), root)), handle)
            })
            .await
            .expect("failed to await init task");
        res?;

        Ok(CephfsMount {
            handle,
            executor: self.executor,
        })
    }
}
