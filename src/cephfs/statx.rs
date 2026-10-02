// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{ffi::CString, mem::MaybeUninit, path::Path, time::SystemTime};

use cephfs_sys::{CEPH_STATX_ALL_STATS, ceph_statx};

use crate::util::{self, check_os_error};

/// Metadata of a file in CephFS.
#[derive(Debug)]
pub struct Statx {
    /// The file's inode number (`stx_inode`)
    pub ino: u64,

    /// The file's mode bits (`stx_mode`)
    pub mode: u16,

    /// The file's UID (`stx_uid`)
    pub uid: u32,
    /// The file's GID (`stx_gid`)
    pub gid: u32,

    /// For device nodes, this holds the device ID they refer to (`stx_rdev`)
    pub device_node: u64,

    /// The ID of the current snapshot (`stx_dev`)
    pub snap_id: u64,

    /// Number of hardlinks referring to the inode (`stx_nlink`)
    pub num_links: u32,

    /// The file's size in bytes (`stx_size`)
    /// For directories, this is the size of all their contents.
    pub size: u64,

    /// The stripe unit of the file's layout (`stx_blksize`)
    pub stripe_unit: u32,

    /// Version tracker for changes to the inode (`stx_version`)
    pub version: u64,

    /// Timestamp of the last access (`stx_atime`)
    pub time_last_access: SystemTime,
    /// Timestamp of the last status change (`stx_ctime`)
    pub time_last_change: SystemTime,
    /// Timestamp of the last modification (`stx_mtime`)
    pub time_last_modification: SystemTime,
    /// Timestamp of the file's creation (`stx_btime`)
    pub time_creation: SystemTime,
}

fn to_system_time(ts: cephfs_sys::timespec) -> crate::Result<std::time::SystemTime> {
    util::to_system_time(ts.tv_sec as u64, ts.tv_nsec as u64)
}

impl<'a> super::CephfsMount<'a> {
    /// Query a file's metadata.
    ///
    /// Equivalent to Linux' `statx(2)`, but some of the fields have
    /// different semantics in CephFS.
    pub async fn statx(&self, path: impl AsRef<Path>) -> crate::Result<Statx> {
        let span = tracing::debug_span!("CephfsMount::statx");
        let path = CString::new(path.as_ref().as_os_str().as_encoded_bytes())?;

        let statx = self
            .do_op(span, move |mount| -> crate::Result<ceph_statx> {
                unsafe {
                    let mut statx = MaybeUninit::uninit();
                    check_os_error(ceph_statx(mount, path.as_ptr(), statx.as_mut_ptr(), CEPH_STATX_ALL_STATS, 0))?;
                    Ok(statx.assume_init())
                }
            })
            .await?;
        Ok(Statx {
            ino: statx.stx_ino,
            mode: statx.stx_mode,
            uid: statx.stx_uid,
            gid: statx.stx_gid,
            device_node: statx.stx_rdev,
            snap_id: statx.stx_dev,
            num_links: statx.stx_nlink,
            size: statx.stx_size,
            stripe_unit: statx.stx_blksize,
            version: statx.stx_version,
            time_last_access: to_system_time(statx.stx_atime)?,
            time_last_change: to_system_time(statx.stx_ctime)?,
            time_last_modification: to_system_time(statx.stx_mtime)?,
            time_creation: to_system_time(statx.stx_btime)?,
        })
    }
}
