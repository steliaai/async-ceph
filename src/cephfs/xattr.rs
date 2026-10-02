// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    ffi::{CString, OsStr},
    path::Path,
};

use cephfs_sys::{CEPH_XATTR_CREATE, CEPH_XATTR_REPLACE, ceph_getxattr, ceph_removexattr, ceph_setxattr};

use crate::util::check_os_error;

/// Flags to control the operation of [`CephfsMount::set_xattr`](super::CephfsMount::set_xattr).
pub enum SetXattrFlags {
    /// Set the xattr regardless of its current state
    None,
    /// Create the xattr, fail if it already exists
    OnlyCreate,
    /// Replace the xattr, fail if it doesn't exist
    OnlyReplace,
}

impl<'a> super::CephfsMount<'a> {
    /// Read the xattr with the given name on the given path.
    ///
    /// Returns its value if present, or `None` if absent.
    /// Equivalent to Linux' `getxattr(2)`, except that ENODATA is mapped to an `Option`.
    pub async fn get_xattr(&self, path: impl AsRef<Path>, name: impl AsRef<OsStr>) -> crate::Result<Option<Vec<u8>>> {
        let span = tracing::debug_span!("CephfsMount::get_xattr");
        let path = CString::new(path.as_ref().as_os_str().as_encoded_bytes())?;
        let name = CString::new(name.as_ref().as_encoded_bytes())?;

        self.do_op(span, move |mount| unsafe {
            loop {
                // Query size
                let ret = ceph_getxattr(mount, path.as_ptr(), name.as_ptr(), core::ptr::null_mut(), 0);
                let size = match usize::try_from(ret) {
                    // xattr exists, but is empty
                    Ok(0) => return Ok(Some(vec![])),
                    // xattr exists and currently has this size
                    Ok(size) => size,
                    // xattr doesn't exist
                    Err(_) if ret == -libc::ENODATA => return Ok(None),
                    // Other error
                    Err(_) => return Err(std::io::Error::from_raw_os_error(-ret).into()),
                };
                let mut data = Vec::<u8>::with_capacity(size);
                // Query value
                let ret = ceph_getxattr(mount, path.as_ptr(), name.as_ptr(), data.as_mut_ptr().cast(), size);
                match usize::try_from(ret) {
                    // Query succeeded, buffer was large enough
                    Ok(size) => {
                        data.set_len(size);
                        return Ok(Some(data));
                    }
                    // xattr doesn't exist (was deleted since the size query)
                    Err(_) if ret == -libc::ENODATA => return Ok(None),
                    // Buffer was too short (xattr was changed since the size query)
                    Err(_) if ret == -libc::ERANGE => continue,
                    // Other error
                    Err(_) => return Err(std::io::Error::from_raw_os_error(-ret).into()),
                }
            }
        })
        .await
    }

    /// Set the xattr with the given name on the given path to the given value.
    ///
    /// `flags` can be used to specify that it should only be set if present or absent.
    /// Equivalent to Linux' `setxattr(2)`.
    pub async fn set_xattr(
        &self,
        path: impl AsRef<Path>,
        name: impl AsRef<OsStr>,
        value: Vec<u8>,
        flags: SetXattrFlags,
    ) -> crate::Result<()> {
        let span = tracing::debug_span!("CephfsMount::set_xattr");
        let path = CString::new(path.as_ref().as_os_str().as_encoded_bytes())?;
        let name = CString::new(name.as_ref().as_encoded_bytes())?;
        let flags = match flags {
            SetXattrFlags::None => 0,
            SetXattrFlags::OnlyCreate => CEPH_XATTR_CREATE as i32,
            SetXattrFlags::OnlyReplace => CEPH_XATTR_REPLACE as i32,
        };

        self.do_op(span, move |mount| unsafe {
            check_os_error(ceph_setxattr(mount, path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), flags))?;
            Ok(())
        })
        .await
    }

    /// Remove the xattr with the given name on the given path.
    ///
    /// Equivalent to Linux' `removexattr(2)`.
    pub async fn remove_xattr(&self, path: impl AsRef<Path>, name: impl AsRef<OsStr>) -> crate::Result<()> {
        let span = tracing::debug_span!("CephfsMount::remove_xattr");
        let path = CString::new(path.as_ref().as_os_str().as_encoded_bytes())?;
        let name = CString::new(name.as_ref().as_encoded_bytes())?;

        self.do_op(span, move |mount| unsafe {
            check_os_error(ceph_removexattr(mount, path.as_ptr(), name.as_ptr()))?;
            Ok(())
        })
        .await
    }
}
