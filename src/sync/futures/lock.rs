//! A "mutex" which only supports `try_lock`
//!
//! As a futures library the eventual call to an event loop should be the only
//! thing that ever blocks, so this is assisted with a fast user-space
//! implementation of a lock that can only have a `try_lock` operation.
//!
//! This code is taken directly from the [futures](https://github.com/rust-lang/futures-rs)
//! crate ecosystem, with small modification to use Loom synchronization primitives. It
//! is provided under the MIT license:
//!
//! ```txt
//! Copyright (c) 2016 Alex Crichton
//! Copyright (c) 2017 The Tokio Authors
//!
//! Permission is hereby granted, free of charge, to any
//! person obtaining a copy of this software and associated
//! documentation files (the "Software"), to deal in the
//! Software without restriction, including without
//! limitation the rights to use, copy, modify, merge,
//! publish, distribute, sublicense, and/or sell copies of
//! the Software, and to permit persons to whom the Software
//! is furnished to do so, subject to the following
//! conditions:
//!
//! The above copyright notice and this permission notice
//! shall be included in all copies or substantial portions
//! of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
//! ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
//! TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
//! PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
//! SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
//! CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
//! OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
//! IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
//! DEALINGS IN THE SOFTWARE.
//! ```

use std::{
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    sync::atomic::Ordering::*,
};

use loom::{
    cell::{MutPtr, UnsafeCell},
    sync::atomic::AtomicBool,
};

/// A "mutex" around a value, similar to `std::sync::Mutex<T>`.
///
/// This lock only supports the `try_lock` operation, however, and does not
/// implement poisoning.
#[derive(Debug)]
pub(crate) struct Lock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

/// Sentinel representing an acquired lock through which the data can be
/// accessed.
pub(crate) struct TryLock<'a, T> {
    lock: &'a Lock<T>,
    ptr: ManuallyDrop<MutPtr<T>>,
}

// The `Lock` structure is basically just a `Mutex<T>`, and these two impls are
// intended to mirror the standard library's corresponding impls for `Mutex<T>`.
//
// If a `T` is sendable across threads, so is the lock, and `T` must be sendable
// across threads to be `Sync` because it allows mutable access from multiple
// threads.
unsafe impl<T: Send> Send for Lock<T> {}
unsafe impl<T: Send> Sync for Lock<T> {}

unsafe impl<T: Send> Send for TryLock<'_, T> {}
unsafe impl<T: Send> Sync for TryLock<'_, T> {}

impl<T> Lock<T> {
    /// Creates a new lock around the given value.
    pub(crate) fn new(t: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(t),
        }
    }

    /// Attempts to acquire this lock, returning whether the lock was acquired or
    /// not.
    ///
    /// If `Some` is returned then the data this lock protects can be accessed
    /// through the sentinel. This sentinel allows both mutable and immutable
    /// access.
    ///
    /// If `None` is returned then the lock is already locked, either elsewhere
    /// on this thread or on another thread.
    pub(crate) fn try_lock(&self) -> Option<TryLock<'_, T>> {
        if !self.locked.swap(true, SeqCst) {
            Some(TryLock {
                lock: self,
                ptr: ManuallyDrop::new(self.data.get_mut()),
            })
        } else {
            None
        }
    }
}

impl<T> Deref for TryLock<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // The existence of `TryLock` represents that we own the lock, so we
        // can safely access the data here.
        unsafe { (*self.ptr).deref() }
    }
}

impl<T> DerefMut for TryLock<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // The existence of `TryLock` represents that we own the lock, so we
        // can safely access the data here.
        //
        // Additionally, we're the *only* `TryLock` in existence so mutable
        // access should be ok.
        unsafe { (*self.ptr).deref() }
    }
}

impl<T> Drop for TryLock<'_, T> {
    fn drop(&mut self) {
        // We need to drop the MutPtr before unlocking the mutex proper,
        // otherwise loom will consider that we are still borrowing self.data
        unsafe { ManuallyDrop::drop(&mut self.ptr) };
        self.lock.locked.store(false, SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::Lock;

    fn model(f: impl Fn() + Send + Sync + 'static) {
        #[cfg(loom)]
        loom::model(f);
        #[cfg(not(loom))]
        f();
    }

    #[test]
    fn smoke() {
        model(|| {
            let a = Lock::new(1);
            let mut a1 = a.try_lock().unwrap();
            assert!(a.try_lock().is_none());
            assert_eq!(*a1, 1);
            *a1 = 2;
            drop(a1);
            assert_eq!(*a.try_lock().unwrap(), 2);
            assert_eq!(*a.try_lock().unwrap(), 2);
        })
    }
}
