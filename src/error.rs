// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error types used by the crate.

use std::ops::{Deref, DerefMut};

/// The main error type of the async-ceph crate used for most librados and librbd APIs.
///
/// This is a thin wrapper around a [`std::io::Error`], and in fact can [`Deref`] into it.
///
/// You can check if the error was emitted by the bindings or librados/librd itself using
/// [`Error::is_rados_error`]. Use the [`kind`](std::io::Error::kind) method (or the raw errno
/// with [`raw_os_error`](std::io::Error::raw_os_error)) to determine how the operation failed.
#[derive(Debug)]
#[repr(transparent)]
pub struct Error(std::io::Error);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_rados_error() {
            f.write_fmt(format_args!("rados/librbd error: {}", self.0))
        } else {
            std::fmt::Display::fmt(&self.0, f)
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self(value)
    }
}

impl From<Error> for std::io::Error {
    fn from(value: Error) -> Self {
        value.0
    }
}

impl From<std::ffi::NulError> for Error {
    fn from(value: std::ffi::NulError) -> Self {
        Error::ffi(value)
    }
}

impl From<std::convert::Infallible> for Error {
    fn from(_value: std::convert::Infallible) -> Self {
        unreachable!("Infallible cannot be soundly constructed")
    }
}

impl Deref for Error {
    type Target = std::io::Error;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Error {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Error type that is used for generial fallible user data to FFI type conversions (typically strings to c-strings).
#[derive(Debug, Clone)]
pub struct FfiConversionError(String);

impl FfiConversionError {
    pub(crate) fn new<E: std::error::Error>(error: E) -> Self {
        Self(format!("FFI conversion error: {error}"))
    }
}

impl std::fmt::Display for FfiConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FfiConversionError {}

impl Error {
    /// Shorthand for creating an [`Error`] caused by an FFI conversion failure.
    pub(crate) fn ffi<E: std::error::Error>(error: E) -> Self {
        Self(std::io::Error::new(std::io::ErrorKind::InvalidInput, FfiConversionError::new(error)))
    }

    /// Shorthand for creating an unexpected / "unreachable" error
    pub(crate) fn unexpected(message: &'static str) -> Self {
        Self(std::io::Error::other(message))
    }

    /// Shorthand for creating an [`Error`] wrapping an [`std::io::Error`] caused by invalid user input.
    #[allow(unused)]
    pub(crate) fn invalid_input<E: Into<Box<dyn std::error::Error + Send + Sync>>>(error: E) -> Self {
        Self(std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
    }

    /// Check if this error is the result of a librados or librbd error.
    ///
    /// This is effectively the same as checking if the error is a raw OS error, i.e.
    /// `self.raw_os_error().is_some()`.
    pub fn is_rados_error(&self) -> bool {
        self.raw_os_error().is_some()
    }

    /// Inspect the underlying cause of this error if it is a [`FfiConversionError`].
    ///
    /// This is the case when user input didn't have a valid representation as the low-level type
    /// accepted by librbd/librados APIs (typically strings that have nul bytes and thus can't be c-strings).
    pub fn as_ffi_conversion_error(&self) -> Option<&FfiConversionError> {
        self.get_ref()?.downcast_ref()
    }
}

/// The main result type of the async-ceph crate, returned by most librados and librbd APIs.
pub type Result<T> = std::result::Result<T, Error>;
