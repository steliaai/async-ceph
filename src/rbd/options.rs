// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    ffi::{CString, c_void},
    ptr::NonNull,
};

use libc::c_int;

use crate::{rbd::features::Features, util::check_os_error};

/// Simple declarative macro to reduce boilerplate when defining a trivial builder struct.
macro_rules! derive_builder {
    (
        $(#[$attrs:meta])*
        $vis:vis struct $name:ident {$(
            $(#[$field_attrs:meta])*
            $field_vis:vis $field_name:ident: $field_ty:ty,
        )*}
    ) => {
        // re-emit struct definition with fields wrapped in Option
        $(#[$attrs])*
        $vis struct $name {$(
            $(#[$field_attrs])*
            $field_vis $field_name: Option<$field_ty>,
        )*}

        impl $name {
            #[doc = "Create an empty"]
            #[doc = stringify!($name)]
            #[doc = "struct."]
            pub fn new() -> Self {
                // not making this a const fn that initializes all fields to `None`,
                // because some fields may have been cfg'd out
                Self::default()
            }

            // add shorthand builder methods
            $(
                $(#[$field_attrs])*
                $field_vis fn $field_name(self, $field_name: impl Into<Option<$field_ty>>) -> Self {
                    Self {
                        $field_name: $field_name.into(),
                        ..self
                    }
                }
            )*
        }
    };
}

derive_builder!(
    /// RBD image options map.
    ///
    /// This is a safe wrapper around `rbd_image_options_t` and related APIs.
    #[derive(Default, Debug, Clone)]
    pub struct ImageOptions {
        /// The [image format](ImageFormat).
        #[doc(alias = "RBD_IMAGE_OPTION_FORMAT")]
        pub format: ImageFormat,
        /// The features that will be enabled for this image.
        ///
        /// Setting this to [`FeatureOptions::Exact`] or a plain [`Features`] value ignores
        /// all default features that the cluster usually sets on creation. It also overwrites
        /// any additive feature options previously set on this struct.
        ///
        /// To enable or disable features on top of the cluster configuration defaults instead, pass
        /// [`FeatureOptions::Delta`] or use the [`enable_features`] and [`disable_features`]
        /// convenience methods.
        ///
        /// [`enable_features`]: ImageOptions::enable_features
        /// [`disable_features`]: ImageOptions::disable_features
        #[doc(alias = "RBD_IMAGE_OPTION_FEATURES")]
        #[doc(alias = "RBD_IMAGE_OPTION_FEATURES_SET")]
        #[doc(alias = "RBD_IMAGE_OPTION_FEATURES_CLEAR")]
        pub features: FeatureOptions,
        /// Dictates the RADOS object size to use for the image, which is 2<sup>order</sup>.
        #[doc(alias = "RBD_IMAGE_OPTION_ORDER")]
        pub order: u64,
        /// The image's [stripe layout](StripeLayout).
        ///
        /// This constitutes the striple unit and size. librbd requires them to be
        /// specified together.
        ///
        /// Note that the image object size, 2<sup>[order](Self::order)</sup>, must be divislbe by the
        /// [`StripeLayout::unit`]. If not, librbd will return an error with
        /// [`std::io::ErrorKind::InvalidInput`].
        #[doc(alias = "RBD_IMAGE_OPTION_STRIPE_UNIT")]
        #[doc(alias = "RBD_IMAGE_OPTION_STRIPE_COUNT")]
        pub stripe_layout: StripeLayout,
        /// Dictates the RADOS object size to use for the journal, which is 2<sup>order</sup>.
        #[doc(alias = "RBD_IMAGE_OPTION_JOURNAL_ORDER")]
        pub journal_order: u64,
        /// The splay width of the journal logs.
        #[doc(alias = "RBD_IMAGE_OPTION_JOURNAL_SPLAY_WIDTH")]
        pub journal_splay_width: u64,
        /// The OSD pool that will be used to store the journal logs.
        #[doc(alias = "RBD_IMAGE_OPTION_JOURNAL_POOL")]
        pub journal_pool: CString,
        /// The OSD pool that will be used to store the image data.
        #[doc(alias = "RBD_IMAGE_OPTION_DATA_POOL")]
        pub data_pool: CString,
        /// Whether to flatten the image, making it independent of its parent.
        #[doc(alias = "RBD_IMAGE_OPTION_FLATTEN")]
        pub flatten: bool,
        /// The image [clone format](CloneFormat).
        #[doc(alias = "RBD_IMAGE_OPTION_CLONE_FORMAT")]
        pub clone_format: CloneFormat,
        /// The [mirror image mode](MirrorImageMode).
        #[doc(alias = "RBD_IMAGE_OPTION_MIRROR_IMAGE_MODE")]
        pub mirror_image_mode: MirrorImageMode,
    }
);

impl ImageOptions {
    /// Additional features to enable for this image on top of the defaults specified by
    /// cluster configuration.
    ///
    /// Setting this overwrites any exact feature options (e.g. [`FeatureOptions::Exact`] passed
    /// to [`features`]) previously set on this struct.
    ///
    /// To specify an absolute set of features to enable that ignores any configuration defaults,
    /// directly set [`features`] with [`FeatureOptions::Exact`] or a plain [`Features`] value.
    ///
    /// [`features`]: ImageOptions::features
    pub fn enable_features(mut self, features: Features) -> Self {
        if let Some(FeatureOptions::Delta { enable, disable }) = &mut self.features {
            *enable |= features;
            *disable &= !features;
        } else {
            self.features = Some(FeatureOptions::Delta {
                enable: features,
                disable: Features::empty(),
            })
        }
        self
    }

    /// Additional features to disable for this image on top of the defaults specified by
    /// cluster configuration.
    ///
    /// Setting this overwrites any exact feature options (e.g. [`FeatureOptions::Exact`] passed
    /// to [`features`]) previously set on this struct.
    ///
    /// To specify an absolute set of features to enable that ignores any configuration defaults,
    /// directly set [`features`] with [`FeatureOptions::Exact`] or a plain [`Features`] value.
    ///
    /// [`features`]: ImageOptions::features
    pub fn disable_features(mut self, features: Features) -> Self {
        if let Some(FeatureOptions::Delta { enable, disable }) = &mut self.features {
            *enable &= !features;
            *disable |= features;
        } else {
            self.features = Some(FeatureOptions::Delta {
                enable: Features::empty(),
                disable: features,
            })
        }
        self
    }

    pub(crate) fn as_sys_repr(&self) -> crate::Result<SysImageOptions> {
        let mut opts = SysImageOptions::new();

        opts.set_u64(rbd_sys::RBD_IMAGE_OPTION_FORMAT as c_int, self.format.map(|f| f as u64))?;
        opts.set_u64(rbd_sys::RBD_IMAGE_OPTION_ORDER as c_int, self.order)?;
        opts.set_u64(rbd_sys::RBD_IMAGE_OPTION_STRIPE_UNIT as c_int, self.stripe_layout.map(|s| s.unit))?;
        opts.set_u64(rbd_sys::RBD_IMAGE_OPTION_STRIPE_COUNT as c_int, self.stripe_layout.map(|s| s.count))?;
        opts.set_u64(rbd_sys::RBD_IMAGE_OPTION_JOURNAL_ORDER as c_int, self.journal_order)?;
        opts.set_u64(rbd_sys::RBD_IMAGE_OPTION_JOURNAL_SPLAY_WIDTH as c_int, self.journal_splay_width)?;
        opts.set_string(rbd_sys::RBD_IMAGE_OPTION_JOURNAL_POOL as c_int, &self.journal_pool)?;
        opts.set_string(rbd_sys::RBD_IMAGE_OPTION_DATA_POOL as c_int, &self.data_pool)?;
        opts.set_u64(rbd_sys::RBD_IMAGE_OPTION_FLATTEN as c_int, self.flatten.map(|f| f as u64))?;
        opts.set_u64(rbd_sys::RBD_IMAGE_OPTION_CLONE_FORMAT as c_int, self.clone_format.map(|f| f as u64))?;
        opts.set_u64(rbd_sys::RBD_IMAGE_OPTION_MIRROR_IMAGE_MODE as c_int, self.mirror_image_mode.map(|m| m as u64))?;

        match self.features {
            Some(FeatureOptions::Exact(f)) => opts.set_u64(rbd_sys::RBD_IMAGE_OPTION_FEATURES as c_int, Some(f.bits()))?,
            Some(FeatureOptions::Delta { enable, disable }) => {
                opts.set_u64(rbd_sys::RBD_IMAGE_OPTION_FEATURES_SET as c_int, Some(enable.bits()))?;
                opts.set_u64(rbd_sys::RBD_IMAGE_OPTION_FEATURES_CLEAR as c_int, Some(disable.bits()))?;
            }
            None => (),
        }

        Ok(opts)
    }
}

/// Specifies the object layout to use for an RBD image.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    /// The original format for a new rbd image (deprecated).
    ///
    /// This format is understood by all versions of librbd and the kernel rbd module,
    /// but does not support newer features like cloning.
    Legacy = 1,
    /// The modern RBD image format, which is supported by librbd and kernel since version 3.11
    /// (except for striping).
    ///
    /// This adds support for cloning and is more easily extensible to allow more features in the future.
    Modern = 2,
}

/// Specifies how an image is mirrored to a peer cluster (if RBD mirroring is configured).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MirrorImageMode {
    /// Use the RBD journaling image feature to ensure point-in-time, crash-consistent
    /// replication between clusters.
    ///
    /// Every write to the RBD image is first recorded to the associated journal before modifying
    /// the actual image. The remote cluster will read from this associated journal and replay the
    /// updates to its local copy of the image. Since each write to the RBD image will result in
    /// two writes to the Ceph cluster, expect write latencies to nearly double while using the
    /// RBD journaling image feature.
    #[doc(alias = "RBD_MIRROR_IMAGE_MODE_JOURNAL")]
    Journal = 0,
    /// Use periodically scheduled or manually created RBD image mirror-snapshots to replicate
    /// crash-consistent RBD images between clusters.
    ///
    /// The remote cluster will determine any data or metadata updates between two mirror-snapshots
    /// and copy the deltas to its local copy of the image. With the help of the RBD fast-diff image
    /// feature, updated data blocks can be quickly determined without the need to scan the full RBD
    /// image. Since this mode is not as fine-grained as journaling, the complete delta between two
    /// snapshots will need to be synced prior to use during a failover scenario. Any partially applied
    /// set of deltas will be rolled back at the moment of failover.
    #[doc(alias = "RBD_MIRROR_IMAGE_MODE_SNAPSHOT")]
    Snapshot = 1,
}

/// Specifies the internal format for tracking cloned images.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloneFormat {
    /// The original (legacy) clone format.
    ///
    /// Requires attaching to protected snapshots that cannot be removed until the clone
    /// is removed or flattened.
    V1 = 1,
    /// Modern clone format.
    ///
    /// Unlike [`V1`](CloneFormat::V1), it allows clones to be attached to any snapshot
    /// and permits removing in-use parent snapshots. However, it is only supported by
    /// Ceph Mimic (v13) or later clients.
    ///
    /// By default, a cluster will only use the v2 format if it is configured to require Mimic
    /// or later clients.
    V2 = 2,
}

/// Specifies the stripe layout of an RBD image.
///
/// For more information about image striping, see
/// [the Ceph striping documentation](https://docs.ceph.com/en/latest/dev/file-striping/).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StripeLayout {
    /// The size (in bytes) of a block of data used in the RAID 0 distribution of the image.
    ///
    /// All stripe units have equal size. The last stripe unit is typically incomplete;
    /// it represents the data at the end of the image as well as unused “space” beyond it
    /// up to the end of the fixed stripe unit size.
    ///
    /// This must be a divisor of the object size (2 to the power of the image order).
    pub unit: u64,
    /// The number of consecutive stripe units that constitute a RAID 0 “stripe” of file data.
    pub count: u64,
}

impl StripeLayout {
    /// Construct a new [`StripeLayout`] from the stripe unit and count.
    pub const fn new(unit: u64, count: u64) -> Self {
        Self { unit, count }
    }
}

/// Ways that image features can be passed to [`ImageOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeatureOptions {
    /// An exact set of features. Ignores defaults from cluster configuration that would
    /// normally be applied to the image.
    Exact(Features),
    /// features to enable/disable on top of the defaults specified by the cluster configuration.
    Delta {
        /// Features to enable.
        enable: Features,
        /// features to disable.
        disable: Features,
    },
}

impl Default for FeatureOptions {
    fn default() -> Self {
        Self::Delta {
            enable: Features::empty(),
            disable: Features::empty(),
        }
    }
}

impl From<Features> for FeatureOptions {
    fn from(value: Features) -> Self {
        Self::Exact(value)
    }
}

impl From<Features> for Option<FeatureOptions> {
    fn from(value: Features) -> Self {
        Some(FeatureOptions::Exact(value))
    }
}

/// Transparent wrapper around a rbd_image_options_t that destroys it when dropped.
#[derive(Debug)]
#[repr(transparent)]
pub(crate) struct SysImageOptions {
    opaque: NonNull<c_void>,
}

unsafe impl Send for SysImageOptions {}
unsafe impl Sync for SysImageOptions {}

impl SysImageOptions {
    fn new() -> Self {
        let mut opaque = std::ptr::null_mut();
        unsafe { rbd_sys::rbd_image_options_create(&mut opaque) };
        Self {
            opaque: NonNull::new(opaque).expect("rbd_image_options not initialized"),
        }
    }

    fn set_string(&mut self, key: c_int, val: &Option<CString>) -> crate::Result<()> {
        let Some(val) = val else { return Ok(()) };

        check_os_error(unsafe { rbd_sys::rbd_image_options_set_string(self.opaque.as_ptr(), key, val.as_ptr()) })?;
        Ok(())
    }

    fn set_u64(&mut self, key: c_int, val: Option<u64>) -> crate::Result<()> {
        let Some(val) = val else {
            return Ok(());
        };
        check_os_error(unsafe { rbd_sys::rbd_image_options_set_uint64(self.opaque.as_ptr(), key, val) })?;
        Ok(())
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.opaque.as_ptr()
    }
}

impl Drop for SysImageOptions {
    fn drop(&mut self) {
        unsafe { rbd_sys::rbd_image_options_destroy(self.opaque.as_ptr()) };
    }
}

#[cfg(all(test, not(loom), not(miri)))]
mod tests {
    use crate::rbd::{
        Features,
        ImageOptions,
        options::{ImageFormat, MirrorImageMode, StripeLayout},
    };

    #[test]
    fn options_set() {
        let opts = ImageOptions::new()
            .format(ImageFormat::Modern)
            .order(23)
            .stripe_layout(StripeLayout::new(1 << 20, 8))
            .journal_order(21)
            .journal_splay_width(None)
            .journal_pool(c"journal-pool".to_owned())
            .enable_features(Features::FAST_DIFF)
            .disable_features(Features::STRIPING_V2)
            .data_pool(c"data-pool".to_owned())
            .flatten(false)
            .clone_format(None)
            .mirror_image_mode(MirrorImageMode::Snapshot);

        opts.as_sys_repr().unwrap();
    }
}
