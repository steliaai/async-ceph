// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#[allow(unused)]
use std::ffi::{CStr, CString, c_char};

pub trait CheckOsError: Copy {
    type Unsigned;

    // not using receiver to prevent method syntax
    fn as_result(value: Self) -> std::io::Result<Self::Unsigned>;
}

macro_rules! check_os_error_impl {
    ($ity:ty, $uty:ty) => {
        impl CheckOsError for $ity {
            type Unsigned = $uty;

            fn as_result(value: Self) -> std::io::Result<Self::Unsigned> {
                if value < 0 {
                    return Err(std::io::Error::from_raw_os_error(-value as i32));
                }

                Ok(value as $uty)
            }
        }
    };
}

check_os_error_impl!(i32, u32);
check_os_error_impl!(i64, u64);
check_os_error_impl!(isize, usize);

/// A short wrapper to put around FFI calls that indicate error by returning a negative value representing errno.
pub fn check_os_error<T: CheckOsError>(value: T) -> std::io::Result<T::Unsigned> {
    CheckOsError::as_result(value)
}

/// Helper for retrying calls to an ffi function that writes to a buffer and exposes the full size
/// whether or not the provided buffer was large enough.
///
/// # Safety
/// The `ffi_call` closure takes two arguments which will be referred to as `buf` as `size_inout`.
///
/// - Before execution, `ffi_call` has write-only access to a `size_inout` byte slice starting at `buf`.
/// - If returning with a non-negative error code, `ffi_call` must have written at least
///   `size_inout` initialized elements to `buf`.
/// - If returning with `-ERANGE` or `-E2BIG`, `size_inout` must be set to the expected number of elements.
#[cfg(feature = "rbd")]
pub unsafe fn get_known_size_arr<T>(ffi_call: impl Fn(*mut T, &mut usize) -> i32, size_hint: usize) -> std::io::Result<Vec<T>> {
    let mut buf: Vec<T> = Vec::with_capacity(size_hint);
    let mut actual_size = buf.capacity();

    loop {
        match ffi_call(buf.as_mut_ptr().cast(), &mut actual_size) {
            e if e == -libc::ERANGE || e == -libc::E2BIG || actual_size > buf.capacity() => {
                buf.reserve(actual_size);
            }
            0 .. => unsafe {
                buf.set_len(actual_size);
                return Ok(buf);
            },
            e => return Err(std::io::Error::from_raw_os_error(-e)),
        }
    }
}

/// Helper for retrying calls to an ffi function that writes a nul-terminated string to a buffer and returns
/// the full size (including the nul terminator) of the string whether or not the provided buffer was large enough.
///
/// # Safety
/// The `ffi_call` closure takes two arguments which will be referred to as `buf` as `size_inout`.
///
/// - Before execution, `ffi_call` has write-only access to a `size_inout` byte slice starting at `buf`.
/// - If returning with a non-negative error code, `ffi_call` must have written a valid c-string with total length
///   `size_inout` to `buf`.
/// - If returning with `-ERANGE` or `-E2BIG`, `size_inout` must be set to the expected number of bytes.
#[cfg(feature = "rbd")]
pub unsafe fn get_known_size_cstr(
    ffi_call: impl Fn(*mut c_char, &mut usize) -> i32,
    size_hint: usize,
) -> std::io::Result<CString> {
    unsafe {
        let buf = get_known_size_arr(|ptr: *mut u8, size| ffi_call(ptr.cast(), size), size_hint)?;
        Ok(CString::from_vec_with_nul_unchecked(buf))
    }
}

/// Helper for retrying calls to an ffi function that writes a nul-terminated string to a buffer and
/// fails if the buffer was too small to hold the string.
///
/// # Safety
/// The `ffi_call` closure takes two arguments which will be referred to as `buf` as `size`.
///
/// - Before execution, `ffi_call` has write-only access to a `size` byte slice starting at `buf`.
/// - If returning with a non-negative error code, `ffi_call` must have written a valid c-string into `buf`.
/// - If returning with `-ERANGE` or `-E2BIG`, the buffer is too small to hold the full string.
pub unsafe fn get_unknown_size_cstr(ffi_call: impl Fn(*mut c_char, usize) -> i32, size_hint: usize) -> std::io::Result<CString> {
    let mut buf: Vec<u8> = Vec::with_capacity(size_hint);
    loop {
        match ffi_call(buf.as_mut_ptr().cast(), buf.capacity()) {
            0 .. => unsafe {
                let len = CStr::from_ptr(buf.as_ptr().cast()).count_bytes() + 1;
                buf.set_len(len);
                return Ok(CString::from_vec_with_nul_unchecked(buf));
            },
            e if e == -libc::ERANGE || e == -libc::E2BIG || e == -libc::ENAMETOOLONG => buf.reserve(2 * buf.capacity()),
            e => return Err(std::io::Error::from_raw_os_error(-e)),
        }
    }
}

#[allow(unused)]
pub fn to_system_time(tv_sec: u64, tv_nsec: u64) -> crate::Result<std::time::SystemTime> {
    std::time::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(tv_sec))
        .and_then(|t| t.checked_add(std::time::Duration::from_nanos(tv_nsec)))
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "failed to convert unix timespec to std::time::SystemTime")
                .into()
        })
}
