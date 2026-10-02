// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Librbd async operations.

use std::os::raw::{c_int, c_void};

use rbd_sys::{rbd_completion_t, rbd_image_t};

use crate::{
    Error,
    Result,
    buf::{BoundedBuf, BoundedBufMut},
    rbd::aio::AsyncOp,
    util::check_os_error,
};

pub type CreateResult<Op> = std::result::Result<Op, (<Op as AsyncOp>::Data, crate::Error)>;

/// Asynchronous read operation.
pub struct Read<B: BoundedBufMut + Send> {
    offset: u64,
    op_flags: c_int,
    buf: B,
}

impl<B: BoundedBufMut + Send> Read<B> {
    pub fn new(buf: B, offset: u64, op_flags: c_int) -> Self {
        Self { offset, op_flags, buf }
    }
}

impl<B: BoundedBufMut + Send> AsyncOp for Read<B> {
    type Data = B;
    type Output = usize;

    unsafe fn submit(&mut self, image: rbd_image_t, completion: rbd_completion_t) -> Result<()> {
        check_os_error(unsafe {
            rbd_sys::rbd_aio_read2(
                image,
                self.offset,
                self.buf.bytes_total(),
                self.buf.stable_mut_ptr().cast(),
                completion,
                self.op_flags,
            )
        })?;
        Ok(())
    }

    fn complete(mut self, result: Result<usize>) -> (Self::Data, Result<Self::Output>) {
        if let Ok(n_read) = &result {
            unsafe { self.buf.set_init(*n_read) }
        }
        (self.buf, result)
    }
}

/// Asynchronous write operation.
pub struct Write<B: BoundedBuf + Send> {
    offset: u64,
    op_flags: c_int,
    buf: B,
}

impl<B: BoundedBuf + Send> Write<B> {
    pub fn new(buf: B, offset: u64, op_flags: c_int) -> Self {
        Self { offset, op_flags, buf }
    }
}

impl<B: BoundedBuf + Send> AsyncOp for Write<B> {
    type Data = B;
    type Output = ();

    unsafe fn submit(&mut self, image: rbd_image_t, completion: rbd_completion_t) -> Result<()> {
        check_os_error(unsafe {
            rbd_sys::rbd_aio_write2(
                image,
                self.offset,
                self.buf.bytes_total(),
                self.buf.stable_ptr().cast(),
                completion,
                self.op_flags,
            )
        })?;
        Ok(())
    }

    fn complete(self, result: Result<usize>) -> (Self::Data, Result<Self::Output>) {
        (self.buf, result.map(|_| ()))
    }
}

/// Asynchronous vectored read operation.
pub struct ReadVectored<B: BoundedBufMut + Send> {
    offset: u64,
    bufs: Vec<B>,
    iovecs: Vec<rbd_sys::iovec>,
}

unsafe impl<B: BoundedBufMut + Send> Send for ReadVectored<B> {}

impl<B: BoundedBufMut + Send> ReadVectored<B> {
    pub fn new(bufs: Vec<B>, offset: u64) -> CreateResult<Self> {
        let iovecs = match iovecs(&bufs) {
            Ok(v) => v,
            Err(e) => return Err((bufs, e)),
        };

        Ok(Self { offset, iovecs, bufs })
    }
}

impl<B: BoundedBufMut + Send> AsyncOp for ReadVectored<B> {
    type Data = Vec<B>;
    type Output = usize;

    unsafe fn submit(&mut self, image: rbd_image_t, completion: rbd_sys::rbd_completion_t) -> Result<()> {
        check_os_error(unsafe {
            rbd_sys::rbd_aio_readv(image, self.iovecs.as_ptr(), self.iovecs.len() as c_int, self.offset, completion)
        })?;
        Ok(())
    }

    fn complete(mut self, result: Result<usize>) -> (Self::Data, Result<Self::Output>) {
        if let Ok(size) = &result {
            let mut init_len = *size;
            for b in &mut self.bufs {
                if init_len == 0 {
                    break;
                }
                let init_pos = init_len.min(b.bytes_total());
                unsafe { b.set_init(init_pos) };
                init_len -= init_pos;
            }
            if init_len != 0 {
                tracing::warn!("wrote more bytes than available, possible OOB write?");
            }
        }

        (self.bufs, result)
    }
}

/// Asynchronous write operation.
pub struct WriteVectored<B: BoundedBuf + Send> {
    offset: u64,
    bufs: Vec<B>,
    iovecs: Vec<rbd_sys::iovec>,
}

unsafe impl<B: BoundedBuf + Send> Send for WriteVectored<B> {}

impl<B: BoundedBuf + Send> WriteVectored<B> {
    pub fn new(bufs: Vec<B>, offset: u64) -> CreateResult<Self> {
        let iovecs = match iovecs(&bufs) {
            Ok(v) => v,
            Err(e) => return Err((bufs, e)),
        };

        Ok(Self { offset, iovecs, bufs })
    }
}

impl<B: BoundedBuf + Send> AsyncOp for WriteVectored<B> {
    type Data = Vec<B>;
    type Output = ();

    unsafe fn submit(&mut self, image: rbd_image_t, completion: rbd_sys::rbd_completion_t) -> Result<()> {
        check_os_error(unsafe {
            rbd_sys::rbd_aio_writev(image, self.iovecs.as_ptr(), self.iovecs.len() as c_int, self.offset, completion)
        })?;
        Ok(())
    }

    fn complete(self, result: Result<usize>) -> (Self::Data, Result<Self::Output>) {
        (self.bufs, result.map(|_| ()))
    }
}

/// Discard operation.
///
/// Signals that the provided range's underlying storage is not used and can be reclaimed by the block device.
pub struct Discard {
    offset: u64,
    len: u64,
}

impl Discard {
    pub fn new(offset: u64, len: u64) -> Self {
        Self { offset, len }
    }
}

impl AsyncOp for Discard {
    type Data = ();
    type Output = ();

    unsafe fn submit(&mut self, image: rbd_image_t, completion: rbd_sys::rbd_completion_t) -> Result<()> {
        check_os_error(unsafe { rbd_sys::rbd_aio_discard(image, self.offset, self.len, completion) })?;
        Ok(())
    }

    fn complete(self, result: Result<usize>) -> (Self::Data, Result<Self::Output>) {
        ((), result.map(|_| ()))
    }
}

bitflags::bitflags! {
    /// Flags that can be specified on a RBD zeroing operation.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct ZeroFlags: c_int {
        /// The default zeroing flags.
        const NONE = 0;
        /// Fully allocate the zeroed extent.
        const THICK_PROVISION = 1; // RBD_WRITE_ZEROES_FLAG_THICK_PROVISION
    }
}

/// Zeroing operation.
pub struct WriteZeroes {
    offset: u64,
    len: usize,
    zero_flags: ZeroFlags,
    op_flags: c_int,
}

impl WriteZeroes {
    pub fn new(offset: u64, len: usize, zero_flags: ZeroFlags, op_flags: c_int) -> Self {
        Self {
            offset,
            len,
            zero_flags,
            op_flags,
        }
    }
}

impl AsyncOp for WriteZeroes {
    type Data = ();
    type Output = ();

    unsafe fn submit(&mut self, image: rbd_image_t, completion: rbd_sys::rbd_completion_t) -> Result<()> {
        check_os_error(unsafe {
            rbd_sys::rbd_aio_write_zeroes(image, self.offset, self.len, completion, self.zero_flags.bits(), self.op_flags)
        })?;
        Ok(())
    }

    fn complete(self, result: Result<usize>) -> (Self::Data, Result<Self::Output>) {
        ((), result.map(|_| ()))
    }
}

/// Write-same operation. Writes the same data `repeat` times.
pub struct WriteSame<B: BoundedBuf + Send> {
    buf: B,
    offset: u64,
    len: usize,
    op_flags: c_int,
}

impl<B: BoundedBuf + Send> WriteSame<B> {
    pub fn new(buf: B, offset: u64, repeat: usize, op_flags: c_int) -> CreateResult<Self> {
        let len = match buf.bytes_init().checked_mul(repeat) {
            // According to RADOS docs, this is the largest write this function can do
            Some(len) if len < i32::MAX as usize => len,
            _ => return Err((buf, Error::invalid_input("WriteSame total write size must be less than 2GiB"))),
        };

        Ok(Self {
            buf,
            offset,
            len,
            op_flags,
        })
    }
}

impl<B: BoundedBuf + Send> AsyncOp for WriteSame<B> {
    type Data = B;
    type Output = ();

    unsafe fn submit(&mut self, image: rbd_image_t, completion: rbd_sys::rbd_completion_t) -> Result<()> {
        check_os_error(unsafe {
            rbd_sys::rbd_aio_writesame(
                image,
                self.offset,
                self.len,
                self.buf.stable_ptr().cast(),
                self.buf.bytes_init(),
                completion,
                self.op_flags,
            )
        })?;
        Ok(())
    }

    fn complete(self, result: Result<usize>) -> (Self::Data, Result<Self::Output>) {
        (self.buf, result.map(|_| ()))
    }
}

/// Asynchronous compare-and-write operation.
pub struct CompareAndWrite<C: BoundedBuf + Send, B: BoundedBuf + Send> {
    cmp: C,
    buf: B,
    offset: u64,
    op_flags: c_int,
    mismatch_off: u64,
}

impl<C: BoundedBuf + Send, B: BoundedBuf + Send> CompareAndWrite<C, B> {
    pub fn new(cmp: C, buf: B, offset: u64, op_flags: c_int) -> CreateResult<Self> {
        if buf.bytes_init() != cmp.bytes_init() {
            return CreateResult::Err((
                (cmp, buf),
                Error::invalid_input("CompareAndWrite compare and write buffers must have the same initialized length"),
            ));
        }

        Ok(Self {
            cmp,
            buf,
            offset,
            op_flags,
            mismatch_off: 0,
        })
    }
}

impl<C: BoundedBuf + Send, B: BoundedBuf + Send> AsyncOp for CompareAndWrite<C, B> {
    type Data = (C, B);
    type Output = std::result::Result<(), u64>;

    unsafe fn submit(&mut self, image: rbd_image_t, completion: rbd_sys::rbd_completion_t) -> Result<()> {
        check_os_error(unsafe {
            rbd_sys::rbd_aio_compare_and_write(
                image,
                self.offset,
                self.cmp.bytes_init(),
                self.cmp.stable_ptr().cast(),
                self.buf.stable_ptr().cast(),
                completion,
                // SAFETY: the AsyncOp is not moved while the operation is in flight
                &mut self.mismatch_off,
                self.op_flags,
            )
        })?;
        Ok(())
    }

    fn complete(self, result: Result<usize>) -> (Self::Data, Result<Self::Output>) {
        let result = match result {
            Ok(_) => Ok(Ok(())),
            Err(e) if e.raw_os_error() == Some(libc::EILSEQ) => Ok(Err(self.mismatch_off)),
            Err(e) => Err(e),
        };

        ((self.cmp, self.buf), result)
    }
}

/// Asynchronous vectored compare-and-write operation.
#[cfg(feature = "rbd_compare_and_write_iovec")]
pub struct CompareAndWriteVectored<C: BoundedBuf + Send, B: BoundedBuf + Send> {
    cmps: Vec<C>,
    cmp_iovecs: Vec<rbd_sys::iovec>,
    bufs: Vec<B>,
    buf_iovecs: Vec<rbd_sys::iovec>,
    offset: u64,
    op_flags: c_int,
    mismatch_off: u64,
}

#[cfg(feature = "rbd_compare_and_write_iovec")]
unsafe impl<C: BoundedBuf + Send, B: BoundedBuf + Send> Send for CompareAndWriteVectored<B, C> {}

#[cfg(feature = "rbd_compare_and_write_iovec")]
impl<C: BoundedBuf + Send, B: BoundedBuf + Send> CompareAndWriteVectored<C, B> {
    pub fn new(cmps: Vec<C>, bufs: Vec<B>, offset: u64, op_flags: c_int) -> CreateResult<Self> {
        let cmp_iovecs = match iovecs(&cmps) {
            Ok(v) => v,
            Err(e) => return Err(((cmps, bufs), e)),
        };
        let buf_iovecs = match iovecs(&bufs) {
            Ok(v) => v,
            Err(e) => return Err(((cmps, bufs), e)),
        };

        Ok(Self {
            cmp_iovecs,
            cmps,
            bufs,
            buf_iovecs,
            offset,
            op_flags,
            mismatch_off: u64::MAX, // used as a sentinel
        })
    }
}

#[cfg(feature = "rbd_compare_and_write_iovec")]
impl<C: BoundedBuf + Send, B: BoundedBuf + Send> AsyncOp for CompareAndWriteVectored<C, B> {
    type Data = (Vec<C>, Vec<B>);
    type Output = std::result::Result<(), u64>;

    unsafe fn submit(&mut self, image: rbd_image_t, completion: rbd_sys::rbd_completion_t) -> Result<()> {
        check_os_error(unsafe {
            rbd_sys::rbd_aio_compare_and_writev(
                image,
                self.offset,
                self.cmp_iovecs.as_ptr(),
                self.cmp_iovecs.len() as c_int,
                self.buf_iovecs.as_ptr(),
                self.buf_iovecs.len() as c_int,
                completion,
                // SAFETY: the AsyncOp is not moved while the operation is in flight
                &mut self.mismatch_off,
                self.op_flags,
            )
        })?;
        Ok(())
    }

    fn complete(self, result: Result<usize>) -> (Self::Data, Result<Self::Output>) {
        let result = match result {
            Ok(_) => Ok(Ok(())),
            Err(e) if e.raw_os_error() == Some(libc::EILSEQ) => Ok(Err(self.mismatch_off)),
            Err(e) => Err(e),
        };

        ((self.cmps, self.bufs), result)
    }
}

fn iovecs<B: BoundedBuf>(bufs: &[B]) -> Result<Vec<rbd_sys::iovec>> {
    if bufs.len() > c_int::MAX as usize {
        return Err(Error::invalid_input(format!(
            "vectored io operations can write to at most {} buffers, got {}",
            c_int::MAX,
            bufs.len()
        )));
    }

    Ok(bufs
        .iter()
        .take(i32::MAX as usize)
        .map(|buf| rbd_sys::iovec {
            iov_base: buf.stable_ptr() as *mut c_void,
            iov_len: buf.bytes_total(),
        })
        .collect())
}
