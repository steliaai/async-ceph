// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Utility traits and types.

use std::{
    borrow::Cow,
    convert::Infallible,
    ffi::{CStr, CString, NulError, OsStr, OsString},
    mem::ManuallyDrop,
    num::NonZero,
    ops::{Deref, DerefMut},
};

mod arc_wait;
mod ffi;

#[doc(inline)]
pub(crate) use arc_wait::ArcWait;

#[doc(inline)]
pub(crate) use ffi::*;

/// Wrapper that is always [`Send`] and [`Sync`] no matter the inner type.
///
/// # Safety
/// Using this type opts out of thread-related soundness guarantees and is thus
/// very error prone.
///
/// Prefer manually implementing the traits on your types if necessary.
/// This is mainly intended for use in type aliases or to quickly transfer
/// e.g. raw pointers across thread boundaries or put them behind shared data structures.
pub(crate) struct UnsafeSendSync<T>(T);

impl<T> UnsafeSendSync<T> {
    #[cfg(feature = "rbd")]
    pub const fn new(inner: T) -> Self {
        Self(inner)
    }

    #[allow(unused)]
    pub fn into_inner(self) -> T {
        self.0
    }
}

unsafe impl<T> Send for UnsafeSendSync<T> {}
unsafe impl<T> Sync for UnsafeSendSync<T> {}

impl<T: Clone> Clone for UnsafeSendSync<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Copy> Copy for UnsafeSendSync<T> {}

impl<T> Deref for UnsafeSendSync<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for UnsafeSendSync<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> From<T> for UnsafeSendSync<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

/// Runs the provided closure when dropped.
pub(crate) struct DropGuard<F: FnOnce()>(ManuallyDrop<F>);

impl<F: FnOnce()> DropGuard<F> {
    pub const fn new(f: F) -> Self {
        Self(ManuallyDrop::new(f))
    }
}

impl<F: FnOnce()> Drop for DropGuard<F> {
    fn drop(&mut self) {
        let f = unsafe { ManuallyDrop::take(&mut self.0) };
        f();
    }
}

/// Like [`TryInto<CString>`], but implemented for more types than in the standard library.
///
/// Provides infallible conversions from:
/// - <code>&[CStr]</code>
/// - [`CString`]
/// - [`Cow<'_, CStr>`]
/// - [`Box<CStr>`]
/// - [`Vec<NonZero<u8>>`]
///
/// Provides fallible conversions returning a [`NulError`] on failure:
/// - From [`str`] types: <code>&[str]</code> [`String`], [`Box<str>`] and [`Cow<'_, str>`].
/// - From [`OsStr`] types: <code>&[OsStr]</code> [`OsString`], [`Box<OsStr>`] and [`Cow<'_, OsStr>`].
/// - From <code>[[u8]]</code> types: <code>&[[u8]]</code>, [`Vec<u8>`], [`Box<\[u8\]>`] and [`Cow<'_, \[u8\]>`].
pub trait TryIntoCString: Sized {
    /// The error type returned on failed conversions.
    ///
    /// The [`std::error::Error`] bound makes wrapping such errors easier.
    type Error: std::error::Error;

    /// Try to convert the value into a [`CString`].
    fn try_into_cstring(self) -> Result<CString, Self::Error>;
}

impl TryIntoCString for &CStr {
    type Error = Infallible;

    /// Converts a <code>&[CStr]</code> into a [`CString`] by copying the contents into a new allocation.
    ///
    /// Never fails.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        Ok(self.into())
    }
}

impl TryIntoCString for CString {
    type Error = Infallible;

    /// Identity conversion, never fails.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        Ok(self)
    }
}

impl TryIntoCString for Box<CStr> {
    type Error = Infallible;

    /// Converts a <code>[Box]<[CStr]></code> into a [`CString`] without copying or allocating.
    ///
    /// Never fails.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        Ok(self.into())
    }
}

impl TryIntoCString for Cow<'_, CStr> {
    type Error = Infallible;

    /// Converts a [`Cow<'_, CStr>`] into a [`CString`], by copying the contents if they are borrowed.
    ///
    /// Never fails.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        Ok(self.into())
    }
}

impl TryIntoCString for Vec<NonZero<u8>> {
    type Error = Infallible;

    /// Converts a <code>[Vec]<[NonZero]<[u8]>></code> into a [`CString`] without copying nor checking for
    /// inner nul bytes.
    ///
    /// Never fails.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        Ok(self.into())
    }
}

impl TryIntoCString for &str {
    type Error = NulError;

    /// Converts a <code>&[str]</code> into a [`CString`] by copying the contents into a new allocation.
    ///
    /// Fails if a nul byte is already present in the string.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        CString::new(self)
    }
}

impl TryIntoCString for String {
    type Error = NulError;

    /// Converts an owned [`String`] into a [`CString`] by appending a trailing 0 byte.
    ///
    /// Fails if such a byte is already present in the string.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        CString::new(self)
    }
}

impl TryIntoCString for Box<str> {
    type Error = NulError;

    /// Converts a boxed [`str`] into a [`CString`] by appending a trailing 0 byte.
    ///
    /// Fails if such a byte is already present in the string.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        CString::new(self.into_string())
    }
}

impl TryIntoCString for Cow<'_, str> {
    type Error = NulError;

    /// Converts a [`Cow<'_, str>`] into a [`CString`] by converting to an owned value and appending a trailing 0 byte.
    ///
    /// Fails if such a byte is already present in the string.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        CString::new(self.into_owned())
    }
}

impl TryIntoCString for &OsStr {
    type Error = NulError;

    /// Converts a <code>&[OsStr]</code> into a [`CString`] by copying the contents into a new allocation.
    ///
    /// Fails if a nul byte is already present in the string.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        CString::new(self.as_encoded_bytes())
    }
}

impl TryIntoCString for OsString {
    type Error = NulError;

    /// Converts an owned [`OsString`] into a [`CString`] by appending a trailing 0 byte.
    ///
    /// Fails if such a byte is already present in the string.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        CString::new(self.into_encoded_bytes())
    }
}

impl TryIntoCString for Box<OsStr> {
    type Error = NulError;

    /// Converts a boxed [`OsStr`] into a [`CString`] by appending a trailing 0 byte.
    ///
    /// Fails if such a byte is already present in the string.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        CString::new(self.into_os_string().into_encoded_bytes())
    }
}

impl TryIntoCString for Cow<'_, OsStr> {
    type Error = NulError;

    /// Converts a [`Cow<'_, OsStr>`] into a [`CString`] by converting to an owned value and appending a trailing 0 byte.
    ///
    /// Fails if such a byte is already present in the string.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        CString::new(self.into_owned().into_encoded_bytes())
    }
}

impl TryIntoCString for &[u8] {
    type Error = NulError;

    /// Converts a <code>&[[u8]]</code> into a [`CString`] by copying the contents into a new allocation.
    ///
    /// Fails if a nul byte is already present in the slice.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        CString::new(self)
    }
}

impl TryIntoCString for Vec<u8> {
    type Error = NulError;

    /// Converts a [`Vec<u8>`] into a [`CString`] by appending a trailing 0 byte.
    ///
    /// Fails if such a byte is already present in the vector.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        CString::new(self)
    }
}

impl TryIntoCString for Box<[u8]> {
    type Error = NulError;

    /// Converts a [`Box<\[u8\]>`] into a [`CString`] by converting to a [`Vec`] and appending a trailing 0 byte.
    ///
    /// Fails if such a byte is already present.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        CString::new(self)
    }
}

impl TryIntoCString for Cow<'_, [u8]> {
    type Error = NulError;

    /// Converts a [`Cow<'_, \[u8\]>`] into a [`CString`] by converting to an owned value and appending a trailing 0 byte.
    ///
    /// Fails if such a byte is already present in the string.
    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        CString::new(self.into_owned())
    }
}

impl<T: TryIntoCString + Clone> TryIntoCString for &T {
    type Error = T::Error;

    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        self.clone().try_into_cstring()
    }
}

impl<T: TryIntoCString + Clone> TryIntoCString for &mut T {
    type Error = T::Error;

    fn try_into_cstring(self) -> Result<CString, Self::Error> {
        self.clone().try_into_cstring()
    }
}
