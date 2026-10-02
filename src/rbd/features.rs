// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Feature defines from include/rbd/features.h

use std::ffi::CString;

use crate::util::check_os_error;

bitflags::bitflags! {
    /// RBD image feature flags.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    // SAFETY: This *MUST* be transparent with u64 for soundness,
    // as we cast u64 pointers to Features and back.
    #[repr(transparent)]
    pub struct Features: u64 {
        const LAYERING = rbd_sys::RBD_FEATURE_LAYERING as u64;
        const STRIPING_V2 = rbd_sys::RBD_FEATURE_STRIPINGV2 as u64;
        const EXCLUSIVE_LOCK = rbd_sys::RBD_FEATURE_EXCLUSIVE_LOCK as u64;
        const OBJECT_MAP = rbd_sys::RBD_FEATURE_OBJECT_MAP as u64;
        const FAST_DIFF = rbd_sys::RBD_FEATURE_FAST_DIFF as u64;
        const DEEP_FLATTEN = rbd_sys::RBD_FEATURE_DEEP_FLATTEN as u64;
        const JOURNALING = rbd_sys::RBD_FEATURE_JOURNALING as u64;
        const DATA_POOL = rbd_sys::RBD_FEATURE_DATA_POOL as u64;
        const OPERATIONS = rbd_sys::RBD_FEATURE_OPERATIONS as u64;
        const MIGRATING = rbd_sys::RBD_FEATURE_MIGRATING as u64;
        const NON_PRIMARY = rbd_sys::RBD_FEATURE_NON_PRIMARY as u64;
        const DIRTY_CACHE = rbd_sys::RBD_FEATURE_DIRTY_CACHE as u64;
    }
}

impl Default for Features {
    fn default() -> Self {
        Self::default()
    }
}

impl Features {
    /// Features that are enabled by default.
    pub const fn default() -> Self {
        Features::from_bits_retain(rbd_sys::RBD_FEATURES_DEFAULT as u64)
    }

    /// True if this feature is enabled by default.
    pub const fn is_default(&self) -> bool {
        (self.bits() & Self::default().bits()) != 0
    }

    /// Features that make an image impossible to access (read/write) for clients that don't understand them.
    pub const fn incompatible() -> Self {
        Features::from_bits_retain(rbd_sys::RBD_FEATURES_INCOMPATIBLE as u64)
    }

    /// True if this feature makes an image impossible to access (read/write) for clients that don't understand them.
    pub const fn is_incompatible(&self) -> bool {
        (self.bits() & Self::incompatible().bits()) != 0
    }

    /// Features that make an image unwritable for clients that don't understand them.
    pub const fn rw_incompatible() -> Self {
        Features::from_bits_retain(rbd_sys::RBD_FEATURES_RW_INCOMPATIBLE as u64)
    }

    /// True if this feature makes an image unwritable for clients that don't understand them.
    pub const fn is_rw_incompatible(&self) -> bool {
        (self.bits() & Self::rw_incompatible().bits()) != 0
    }

    /// Features that can be dynamically enabled or disabled.
    pub const fn mutable() -> Self {
        Features::from_bits_retain(rbd_sys::RBD_FEATURES_MUTABLE as u64)
    }

    /// True if this feature can be dynamically enabled or disabled.
    pub const fn is_mutable(&self) -> bool {
        (self.bits() & Self::mutable().bits()) != 0
    }

    /// Features that can be dynamically enabled or disabled, but not by the user.
    pub const fn mutable_internal() -> Self {
        Features::from_bits_retain(rbd_sys::RBD_FEATURES_MUTABLE_INTERNAL as u64)
    }

    /// True if this feature can be dynamically enabled or disabled, but not by the user.
    pub const fn is_mutable_internal(&self) -> bool {
        (self.bits() & Self::mutable_internal().bits()) != 0
    }

    /// Features that be disabled dynamically, but not enabled.
    pub const fn disable_only() -> Self {
        Features::from_bits_retain(rbd_sys::RBD_FEATURES_DISABLE_ONLY as u64)
    }

    /// True if this feature can be disabled dynamically, but not enabled.
    pub const fn is_disable_only(&self) -> bool {
        (self.bits() & Self::disable_only().bits()) != 0
    }

    /// Features that only work when used with a single writing client.
    pub const fn single_client() -> Self {
        Features::from_bits_retain(rbd_sys::RBD_FEATURES_SINGLE_CLIENT as u64)
    }

    /// True if this feature only works when a single client is writing the image.
    pub const fn is_single_client(&self) -> bool {
        (self.bits() & Self::single_client().bits()) != 0
    }

    /// Features that will be implicitly enabled.
    pub const fn implicit_enable() -> Self {
        Features::from_bits_retain(rbd_sys::RBD_FEATURES_IMPLICIT_ENABLE as u64)
    }

    /// True if this feature will be implicitly enabled.
    pub const fn is_implicit_enable(&self) -> bool {
        (self.bits() & Self::implicit_enable().bits()) != 0
    }

    /// Features that cannot be directly controlled by the user.
    pub const fn internal() -> Self {
        Features::from_bits_retain(rbd_sys::RBD_FEATURES_INTERNAL as u64)
    }

    /// True if this is an internal feature that cannot be directly controlled by the user.
    pub const fn is_internal(&self) -> bool {
        (self.bits() & Self::internal().bits()) != 0
    }
}

impl std::fmt::Display for Features {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut buf: Vec<u8> = Vec::with_capacity(128);

        loop {
            let mut size_with_nul = buf.capacity();
            let err = unsafe { rbd_sys::rbd_features_to_string(self.bits(), buf.as_mut_ptr().cast(), &mut size_with_nul) };

            match err {
                0 .. => {
                    unsafe { buf.set_len(size_with_nul - 1) };
                    return f.write_str(str::from_utf8(&buf).map_err(|_| std::fmt::Error)?);
                }
                e if e == -libc::ERANGE => buf.reserve(size_with_nul),
                _ => return Err(std::fmt::Error),
            }
        }
    }
}

impl std::str::FromStr for Features {
    type Err = std::io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let c_str = CString::new(s).map_err(|_| std::io::ErrorKind::InvalidData)?;
        let mut features: Self = Features::empty();

        check_os_error(unsafe { rbd_sys::rbd_features_from_string(c_str.as_ptr(), (&raw mut features).cast()) })?;
        Ok(features)
    }
}

bitflags::bitflags! {
    /// RBD operation feature flags.
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct OpFeatures: u64 {
        const CLONE_PARENT = rbd_sys::RBD_OPERATION_FEATURE_CLONE_PARENT as u64;
        const CLONE_CHILD = rbd_sys::RBD_OPERATION_FEATURE_CLONE_CHILD as u64;
        const GROUP = rbd_sys::RBD_OPERATION_FEATURE_GROUP as u64;
        const SNAP_TRASH = rbd_sys::RBD_OPERATION_FEATURE_SNAP_TRASH as u64;
    }
}

#[cfg(all(test, not(loom), not(miri)))]
mod tests {
    use crate::rbd::features::Features;

    #[test]
    fn features_str_repr() {
        for feat in Features::all().iter() {
            let str_rep = feat.to_string();
            println!("feat: {str_rep}");
            assert_eq!(str_rep.parse::<Features>().unwrap(), feat)
        }

        let str_defaults = Features::default().to_string();
        println!("defaults: {str_defaults}");
        assert_eq!(str_defaults.parse::<Features>().unwrap(), Features::default())
    }
}
