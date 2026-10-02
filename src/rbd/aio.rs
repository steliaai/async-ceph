// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    ffi::c_void,
    ptr,
    sync::{Arc, Mutex},
};

use futures_channel::oneshot;
use rbd_sys::{
    rbd_aio_create_completion,
    rbd_aio_get_return_value,
    rbd_aio_release,
    rbd_callback_t,
    rbd_completion_t,
    rbd_image_t,
};
use tracing::Instrument;

use crate::{Result, rbd::Image, util::check_os_error};

pub(super) mod ops;

pub trait AsyncOp: Sized + Send + 'static {
    /// Data temporarily owned by the async operation for its duration.
    type Data;
    /// Output type when the async operation succeeds.
    type Output;

    /// # Safety
    ///
    /// - The function must be called with a valid `rbd_image_t` as well as a `rbd_completion_t` obtained
    ///   from [`create_rbd_completion`].
    /// - The function is expected to have successfully registered the completion with a rbd aio function
    ///   if *and only if* it returns [`Ok`].
    unsafe fn submit(&mut self, image: rbd_image_t, completion: rbd_completion_t) -> Result<()>;

    /// What to do when the operation completes, either successfully or with an error.
    fn complete(self, result: Result<usize>) -> (Self::Data, Result<Self::Output>);
}

/// Submit a librbd [`AsyncOp`], returning a future that can be awaited to get its result.
///
/// Note that the operation is submitted immediately, not when the future is first polled.
pub fn aio_submit_op<Op: AsyncOp>(
    image: &Image<'_>,
    op: Op,
) -> impl Future<Output = (Op::Data, Result<Op::Output>)> + Send + use<Op> {
    let span = tracing::debug_span!("aio_submit", op = std::any::type_name::<Op>());

    let image = image.handle().clone();
    let future = unsafe { aio_submit_fn((op, image), move |(op, image), comp| op.submit(image.as_ptr(), comp)) };
    async move {
        let ((op, _), completion_result) = future.await;
        let (data, res) = op.complete(completion_result);
        if let Err(e) = &res {
            tracing::error!("aio operation failed: {e}");
        }
        (data, res)
    }
    .instrument(span)
}

/// Wrap code that invokes asynchronous librbd APIs taking a [`rbd_completion_t`] using a completion handler.
///
/// This should be limited to "quick and dirty" AIO operations that don't warrant a full [`AsyncOp`] implementation.
///
/// # Safety
/// The provided closure must call a single `rbd_aio` API function taking a [`rbd_completion_t`] and forward
/// its return value.
///
/// Returning a different return value may lead to memory leaks or use-after-frees.
pub unsafe fn aio_submit_ffi<T, F>(data: T, op: F) -> impl Future<Output = (T, Result<usize>)> + Send + 'static
where
    T: Send + 'static,
    F: FnOnce(&mut T, rbd_completion_t) -> i32, {
    unsafe {
        aio_submit_fn(data, |data, comp| {
            check_os_error(op(data, comp))?;
            Ok(())
        })
    }
}

/// Shared implementation for [`aio_submit_op`] and [`aio_submit_ffi`].
///
/// # Safety
/// The closure is expected to have successfully registered the completion with a rbd aio function
/// if *and only if* it returns [`Ok`].
unsafe fn aio_submit_fn<T, F>(data: T, op: F) -> impl Future<Output = (T, Result<usize>)> + Send + 'static
where
    T: Send + 'static,
    F: FnOnce(&mut T, rbd_completion_t) -> Result<()>, {
    let (tx, rx) = oneshot::channel();

    let data = Arc::new(Mutex::new(data));
    let op_data_ptr = Box::into_raw(Box::new(OpData { data: data.clone(), tx }));

    let submit_result = create_rbd_completion(Some(completion_handler::<T>), op_data_ptr as *mut _)
        // SAFETY: rbd_aio_release is only invoked if `op` returns `Err`.
        .and_then(|comp| op(&mut *data.try_lock().unwrap(), comp).inspect_err(|_| unsafe { rbd_aio_release(comp) }))
        // SAFETY: on error, the async operation was not queued with librbd so free the op data
        .inspect_err(|_| unsafe { drop(Box::from_raw(op_data_ptr)) });

    fn take_data<T>(data: Arc<Mutex<T>>) -> T {
        Arc::into_inner(data).expect("leaked Arc").into_inner().expect("poisoned")
    }

    async move {
        match submit_result {
            Ok(_) => {}
            Err(e) => return (take_data(data), Err(e)),
        };

        let completion_result = rx.await.expect("tx dropped before sending");
        (take_data(data), check_os_error(completion_result).map_err(Into::into))
    }
}

/// Simple wrapper around `rbd_aio_create_completion`.
pub fn create_rbd_completion(handler: rbd_callback_t, arg: *mut c_void) -> Result<rbd_completion_t> {
    let mut completion = ptr::null_mut();
    check_os_error(unsafe { rbd_aio_create_completion(arg.cast(), handler, &mut completion) })?;
    Ok(completion)
}

unsafe extern "C" fn completion_handler<T>(cb: rbd_completion_t, arg: *mut c_void) {
    let (result, handler_data) = unsafe {
        let result = rbd_aio_get_return_value(cb);
        rbd_aio_release(cb);

        (result, *Box::<OpData<T>>::from_raw(arg as *mut _))
    };

    // data must be dropped before we send to ensure the Arc is unique when we `into_inner` it in
    // aio_submit_fn.
    drop(handler_data.data);
    let _ = handler_data.tx.send(result);
}

struct OpData<T> {
    data: Arc<Mutex<T>>,
    tx: oneshot::Sender<isize>,
}
