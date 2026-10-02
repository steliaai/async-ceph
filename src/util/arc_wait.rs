// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    mem::ManuallyDrop,
    ops::Deref,
    pin::Pin,
    ptr::NonNull,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
};

use crate::{
    sync::{
        atomic::{self, AtomicU64, Ordering},
        futures::AtomicWaker,
        hint::spin_loop,
        thread::{self, Thread},
    },
    util::DropGuard,
};

/// Shared inner data of an [`ArcWait`].
///
/// # Refcounting strategy
///
/// To be able to wake the thread waiting on uniqueness, it must be possible to safely
/// access `waker` after decrementing the refcount. To avoid use-after-frees, we tag the
/// upper two bits of the refcount with a state consisting of:
///
/// - `WAITER_NONE` (0): There is no thread/task waiting on uniqueness.
/// - `WAITER_SET` (1): A thread/task is waiting on uniqueness and has not been signaled yet.
/// - `WAITER_DONE` (2): The thread/task waiting on uniqueness has been signaled.
///
/// Wait functions transition from `NONE` to `SET`, then spin until `DONE` before transitioning
/// the state back to `NONE`. The `ArcWaitInner` can only be dropped or unboxed if the state is `NONE`.
///
/// This allows us to:
///
/// - Prevent the aforementioned data race.
///
/// - Prevent concurrent calls of uniqueness wait functions, which would deadlock.
///
/// - Leak the ArcWaitInner if the last ArcWait is dropped while the state is not WAITER_NONE.
///   This protects against the user calling `forget` on the future returned by [`ArcWait::wait_unique`]
///   after it has been polled.
///
/// - Only try to wake the waiting thread/task if there actually is one. This means that there
///   is zero overhead to cloning/dropping the [`ArcWait`] when there is no waiter, even when
///   the refcount becomes unique.
#[repr(C)]
struct ArcWaitInner<T: ?Sized> {
    /// The refcount, tagged with the waiter state.
    tagged_rc: AtomicU64,
    /// The waker used to wake the thread/task waiting on uniqueness.
    waker: AtomicWaker,
    /// The user data.
    data: T,
}

/// Portion of the packed atomic state that stores the reference count.
const REFCOUNT_BITS: u64 = u64::MAX >> 2;
/// Maximum valid reference count. Increasing the count past this will
/// lead to a process abort.
///
/// This was chosen to be half the refcount bits, so a large safety margin
/// is available for concurrent increments of the refcount until the
/// process is actually aborted.
const MAX_REFCOUNT: u64 = REFCOUNT_BITS / 2;
/// Bits storing the state of threads/tasks waiting on uniqueness.
const WAITER_BITS: u64 = !REFCOUNT_BITS;
/// No waker/waiter is registered.
const WAITER_NONE: u64 = 0;
/// A waiter is registered.
const WAITER_SET: u64 = 1u64.rotate_right(2);
/// The waiter's waker was woken up.
const WAITER_DONE: u64 = 2u64.rotate_right(2);

/// [`Arc`](std::sync::Arc)-like reference counted pointer that allows waiting until all clones have been dropped.
///
/// Unlike [`Arc`](std::sync::Arc), it does not support weak references.
#[repr(transparent)]
pub(crate) struct ArcWait<T: ?Sized> {
    inner: NonNull<ArcWaitInner<T>>,
}

unsafe impl<T: ?Sized + Send> Send for ArcWait<T> {}
unsafe impl<T: ?Sized + Sync> Sync for ArcWait<T> {}

impl<T> ArcWait<T> {
    pub fn new(data: T) -> Self {
        let inner = ArcWaitInner {
            tagged_rc: AtomicU64::new(1),
            waker: AtomicWaker::new(),
            data,
        };
        // SAFETY: Box::into_raw returns a non-null pointer
        let inner = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(inner))) };
        Self { inner }
    }

    /// If this is the only [`ArcWait`] pointing to its underlying data, returns it as an owned value.
    #[allow(unused)]
    pub fn try_unwrap(this: Self) -> Result<T, Self> {
        if Self::is_unique(&this) {
            // synchronize with the Drops of other ArcWaits before reclaiming the allocation
            atomic::fence(Ordering::Acquire);

            let this = ManuallyDrop::new(this);
            let inner = unsafe { Box::from_raw(this.inner.as_ptr()) };
            Ok(inner.data)
        } else {
            Err(this)
        }
    }

    /// Wait until this is the only [`ArcWait`] pointing to its underlying data, then returns it
    /// as an owned value.
    ///
    /// # Panics
    ///
    /// If two [`ArcWait`]s pointing to the same data call this or other wait functions concurrently.
    #[allow(unused)]
    pub async fn wait_unwrap(mut this: Self) -> T {
        Self::wait_unique(&mut this).await;
        match Self::try_unwrap(this) {
            Ok(value) => value,
            Err(_) => unreachable!("wait_unique returned before ArcWait was unique"),
        }
    }

    /// Block until this is the only [`ArcWait`] pointing to its underlying data, then returns it
    /// as an owned value.
    ///
    /// # Panics
    ///
    /// If two [`ArcWait`]s pointing to the same data call this or other wait functions concurrently.
    #[allow(unused)]
    pub fn blocking_wait_unwrap(mut this: Self) -> T {
        Self::blocking_wait_unique(&mut this);
        match Self::try_unwrap(this) {
            Ok(value) => value,
            Err(_) => unreachable!("blocking_wait_unique returned before ArcWait was unique"),
        }
    }
}

impl<T: ?Sized> ArcWait<T> {
    fn inner(&self) -> &ArcWaitInner<T> {
        unsafe { self.inner.as_ref() }
    }

    fn refcount(tagged_rc: u64) -> u64 {
        tagged_rc & REFCOUNT_BITS
    }

    fn waiter_state(tagged_rc: u64) -> u64 {
        tagged_rc & WAITER_BITS
    }

    fn rc_relaxed(&self) -> u64 {
        Self::refcount(self.inner().tagged_rc.load(Ordering::Relaxed))
    }

    /// Attempt to reserve the waker slot.
    ///
    /// Returns `false` if the refcount became unique during this process, in
    /// which case the waker slot was not reserved.
    ///
    /// # Panics
    ///
    /// If the waker slot is already locked.
    fn reserve_waker(&self) -> bool {
        self.inner()
            .tagged_rc
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |tagged_rc| {
                if Self::waiter_state(tagged_rc) != WAITER_NONE {
                    panic!("Another thread/task is currently waiting for this ArcWait to become unique");
                }
                (tagged_rc != 1).then_some(tagged_rc | WAITER_SET)
            })
            .is_ok()
    }

    /// Check if no other live [`ArcWait`] points to the same data.
    ///
    /// This function returning `true` does not establish a happens-before relationship
    /// with the `Drop` of other clones; an explicit [`Acquire`](Ordering::Acquire) fence
    /// is required for this.
    ///
    /// Note that the result can only be meaningfully acted upon if `this` is a unique reference.
    /// Otherwise, nothing would stop it from being concurrently cloned.
    pub fn is_unique(this: &Self) -> bool {
        this.inner().tagged_rc.load(Ordering::Relaxed) == WAITER_NONE | 1
    }

    /// Wait until this is the only [`ArcWait`] pointing to the inner data.
    ///
    /// This functions synchronizes with all modifications of the data performed before `this`
    /// is made unique.
    ///
    /// # Cancellation
    ///
    /// This future is cancel safe when dropped. However, leaking it (via [`std::mem::forget`] or other
    /// methods) is not, and will:
    /// - Prevent the memory allocated by `this` (and by extension its value) from being freed;
    /// - Hold the waiter lock, meaning that all future polls of [`Self::wait_unique`] and calls to
    ///   [`Self::blocking_wait_unique`] will panic.
    ///
    /// # Panics
    ///
    /// If two [`ArcWait`]s pointing to the same data poll this future or call other wait functions
    /// concurrently.
    pub async fn wait_unique(this: &mut Self) {
        WaitUnique::new(this).await;
    }

    /// Block until this is the only [`ArcWait`] pointing to the inner data.
    ///
    /// This functions synchronizes with all modifications of the data performed before `this`
    /// is made unique.
    ///
    /// # Panics
    ///
    /// If two [`ArcWait`]s pointing to the same data call this or other wait functions concurrently.
    pub fn blocking_wait_unique(this: &mut Self) {
        // First reserve the waker slot (WAITER_NONE -> WAITER_SET).
        //
        // If returns false the refcount is 1 and we have nothing to do besides synchronizing.
        if !this.reserve_waker() {
            atomic::fence(Ordering::Acquire);
            return;
        }

        // Register a waker that will unpark the current thread and wait until the refcount
        // becomes unique.
        //
        // This is safe as the `AtomicWaker` does not use `thread::park` internally, so the
        // thread's token can't be consumed before the park below.
        let inner = this.inner();
        inner.waker.register(&ThreadWaker::current());
        while this.rc_relaxed() != 1 {
            thread::park();
        }

        // Spin loop to avoid exiting before the Drop waking us is done using the waker,
        // then transition the state back to WAITER_NONE.
        while Self::waiter_state(inner.tagged_rc.load(Ordering::Relaxed)) != WAITER_DONE {
            spin_loop();
        }
        atomic::fence(Ordering::Acquire);
        inner.tagged_rc.store(WAITER_NONE | 1, Ordering::Relaxed);
    }

    /// Turns this [`ArcWait`] into a plain pointer to its data.
    ///
    /// # Warning
    ///
    /// This leaks resources unless reclaimed by [`ArcWait::from_raw`].
    #[allow(unused)]
    pub fn into_raw(this: Self) -> *const T {
        let this = ManuallyDrop::new(this);
        Self::as_raw(&this)
    }

    /// Get a raw pointer to this [`ArcWait`]'s inner data. Does not increment the reference count.
    ///
    /// If `this` is then leaked via [`std::mem::forget`] or [`ManuallyDrop`], this pointer
    /// can be safely reclaimed using [`ArcWait::from_raw`].
    #[allow(unused)]
    pub fn as_raw(this: &Self) -> *const T {
        &this.inner().data
    }

    /// Creates an [`ArcWait`] from a pointer previously obtained from [`ArcWait::into_raw`] or [`ArcWait::as_raw`].
    ///
    /// # Safety
    ///
    /// - `ptr` must have been obtained from [`ArcWait::into_raw`] or [`ArcWait::as_raw`].
    /// - `ptr` can only be passed to this function once.
    /// - If obtained from `as_raw`, the "parent" [`ArcWait`] must have been leaked via [`std::mem::forget`].
    #[allow(unused)]
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        // The soundness of this method may be hard to verify, specifically regarding
        // offset calculation for unsized types. For more details see `data_offset` in std's
        // `Arc` implementation: https://doc.rust-lang.org/src/alloc/sync.rs.html#4204

        // SAFETY:
        // - `ptr` is non-null and points to a valid `T` as it comes from into_raw
        // - `ptr` is not mutably aliased (other ArcWaits only expose immutable refs)
        let align = align_of_val(unsafe { &*ptr });
        // SAFETY: by repr(C) of `ArcWaitInner`'s layout
        let offset = size_of::<ArcWaitInner<()>>().next_multiple_of(align);
        // SAFETY: by the fact that `ptr` comes from `into_raw`
        let inner = unsafe { NonNull::new_unchecked(ptr.byte_sub(offset) as *mut ArcWaitInner<T>) };

        ArcWait { inner }
    }
}

impl<T: ?Sized> Drop for ArcWait<T> {
    fn drop(&mut self) {
        let inner = self.inner();
        let old_rc = inner.tagged_rc.fetch_sub(1, Ordering::Release);

        // Refcount is now zero, we are responsible for freeing the allocation and data
        //
        // Note that we leak the ArcWaitInner if the state is not WAITER_NONE. This can
        // only happen if the user leaks the future returned by `wait_unique` after it
        // has been polled once and transitioned the waiter state to WAITER_SET. This
        // allows the user to reclaim ownership of the clone they called `wait_unique`
        // on and drop it early. If another thread is in the waker.wake() block below
        // when this happens, we might free `inner` before it's done using the waker.
        //
        // Similarly, even if the state is WAITER_DONE, it's unsafe to free: After leaking
        // `wait_unique` as above the user could create another clone and drop it, setting
        // WAITER_DONE while a thread is still suspended in `waker.wake()` below.
        if old_rc == WAITER_NONE | 1 {
            atomic::fence(Ordering::Acquire);
            unsafe { drop(Box::from_raw(self.inner.as_ptr())) };
        }
        // Refcount of 2 observed with a thread/task waiting for uniqueness, wake it.
        // Note that we are able to access `inner` as the allocation cannot be claimed
        // until the state transitions back to WAITER_NONE (and this only happens with
        // acknowledgement of the waiter).
        else if old_rc == WAITER_SET | 2 {
            // Transition to WAITER_DONE. We do this in a drop guard in case the waker panics.
            //
            // This needs to be a `compare_exchange` instead of a store, since by forgetting
            // the WaitUnique future user code may start re-incrermenting the refcount
            //
            // The ordering is `Release` to synchronize with the `Acquire` in
            // uniqueness wait functions, `try_unwrap` and the WaitUnique destructor.

            // If we borrow `inner` inside the drop guard, miri considers it to still be borrowed
            // after the CAS, which leads it to report a Stacked Borrows violation.
            //
            // Capturing only the refcount in the closure seems to fix this.
            let tagged_rc = &inner.tagged_rc;
            let _clear_waiter = DropGuard::new(|| {
                let _ = tagged_rc.compare_exchange(WAITER_SET | 1, WAITER_DONE | 1, Ordering::Release, Ordering::Relaxed);
            });

            inner.waker.wake();
        }
    }
}

impl<T: ?Sized> Deref for ArcWait<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner().data
    }
}

impl<T: ?Sized> Clone for ArcWait<T> {
    fn clone(&self) -> Self {
        let old_rc = self.inner().tagged_rc.fetch_add(1, Ordering::Relaxed);

        // It's unsound for the refcount to overflow REFCOUNT_BITS, so prevent that by
        // aborting well before it happens. Panicking is not enough as it can be handled.
        //
        // This is the same strategy that `std` uses to protect against refcount overflow in
        // the `Arc` implementation: https://doc.rust-lang.org/src/alloc/sync.rs.html#2405
        if Self::refcount(old_rc) > MAX_REFCOUNT {
            std::process::abort();
        }

        Self { inner: self.inner }
    }
}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for ArcWait<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ArcWait").field(&&self.inner().data).finish()
    }
}

impl<T: ?Sized + std::fmt::Display> std::fmt::Display for ArcWait<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.deref(), f)
    }
}

impl<T: ?Sized> std::fmt::Pointer for ArcWait<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Pointer::fmt(&ArcWait::as_raw(self), f)
    }
}

struct ThreadWaker(Thread);

impl ThreadWaker {
    fn current() -> Waker {
        Waker::from(Arc::new(ThreadWaker(thread::current())))
    }
}

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

#[derive(PartialEq)]
enum WaitUniqueState {
    /// Waker slot needs to be reserved by calling ArcWait::reserve_waker.
    Reserving,
    /// Waker slot has been reserved, waiting for uniqueness.
    Waiting,
    /// Done waiting for uniqueness.
    Done,
}

/// Future returned by [`ArcWait::wait_unique`].
struct WaitUnique<'a, T: ?Sized> {
    arc: &'a ArcWait<T>,
    state: WaitUniqueState,
}

impl<'a, T: ?Sized> WaitUnique<'a, T> {
    fn new(arc: &'a ArcWait<T>) -> Self {
        Self {
            arc,
            state: WaitUniqueState::Reserving,
        }
    }

    /// Spin until the Drop that waked us up sets the state to WAITER_DONE, then set it to
    /// WAITER_NONE and transition to [`WaitUniqueState::Done`].
    fn spin_done(&mut self) -> Poll<()> {
        while self.arc.inner().tagged_rc.load(Ordering::Relaxed) & WAITER_BITS != WAITER_DONE {
            spin_loop();
        }
        atomic::fence(Ordering::Acquire);
        self.arc.inner().tagged_rc.store(WAITER_NONE | 1, Ordering::Relaxed);
        self.state = WaitUniqueState::Done;
        Poll::Ready(())
    }
}

impl<T: ?Sized> Future for WaitUnique<'_, T> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.state {
            // If polled after done just return `Ready` like a fused future.
            WaitUniqueState::Done => return Poll::Ready(()),
            // Try to transition to WAITER_SET, and early exit if the refcount is already 1.
            WaitUniqueState::Reserving => {
                if !self.arc.reserve_waker() {
                    atomic::fence(Ordering::Acquire);
                    self.state = WaitUniqueState::Done;
                    return Poll::Ready(());
                }
                self.state = WaitUniqueState::Waiting;
            }
            _ => {}
        }

        // Check uniqueness before registration to avoid it if possible
        if self.arc.rc_relaxed() == 1 {
            return self.spin_done();
        }

        self.arc.inner().waker.register(cx.waker());

        // We must check uniqueness *after* registering to missed wakeups.
        if self.arc.rc_relaxed() == 1 {
            self.spin_done()
        } else {
            Poll::Pending
        }
    }
}

impl<T: ?Sized> Drop for WaitUnique<'_, T> {
    fn drop(&mut self) {
        // If the future is dropped before the waker was reserved or after it's done,
        // there's nothing to do
        if self.state != WaitUniqueState::Waiting {
            return;
        }
        // Otherwise, we have to transition the state back to WAITER_NONE, but can only do so
        // if the refcount is not 1. If it is, we have just become unique and the last ArcWait
        // dropped might be in the process of waking us, so we must wait until it's done.
        let refcount = &self.arc.inner().tagged_rc;
        let mut rc = refcount.load(Ordering::Relaxed);
        while rc & WAITER_BITS != WAITER_DONE {
            // If the count isn't 1, we can try clearing the state ourselves with a CAS.
            // This is safe as if the CAS succeeds we're definitely not being waken up.
            if rc & REFCOUNT_BITS != 1 {
                let new_rc = (rc & REFCOUNT_BITS) | WAITER_NONE;
                match refcount.compare_exchange_weak(rc, new_rc, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => return,
                    Err(new_rc) => rc = new_rc,
                }
            }
            // Otherwise, the last ArcWait to be dropped is currently waking us up, so we have to
            // wait until it's done and transitions the state to WAITER_DONE.
            else {
                spin_loop();
                rc = refcount.load(Ordering::Relaxed);
            }
        }
        // An acquire fence is needed here to synchronize with the `Drop` that was trying to wake
        // us and the final `Drop` that observes the value written below (miri catches this!).
        atomic::fence(Ordering::Acquire);
        // We don't need Release on the final store, because the borrowed ArcWait is now unique.
        // Any further operation on it happens after this Drop and thus the store.
        refcount.store(WAITER_NONE | 1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::ArcWait;
    use crate::{
        sync::{Arc, thread},
        util::arc_wait::{WaitUnique, WaitUniqueState},
    };
    use rstest::rstest;
    use std::{pin::Pin, time::Duration};

    fn secs(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    fn model(f: impl Fn() + Send + Sync + 'static) {
        // For normal test runs or runs under ASAN/TSAN, try the test a bunch
        #[cfg(not(any(loom, miri)))]
        for _ in 0 .. 1000 {
            f();
        }
        // miri is about 1000x slower, so keep the number of iterations down
        #[cfg(miri)]
        for _ in 0 .. 50 {
            f();
        }
        #[cfg(loom)]
        loom::model(f);
    }

    // Unlike `model`, just run the function once (in a loom context if required)
    //
    // Intended for single-threaded tests that we may still want to run under loom
    fn test(f: impl Fn() + Send + Sync + 'static) {
        #[cfg(not(loom))]
        f();
        #[cfg(loom)]
        loom::model(f);
    }

    #[rstest]
    #[timeout(secs(60))]
    pub fn smoke_test() {
        test(|| {
            let mut a1 = ArcWait::new(Arc::new(123));
            assert!(ArcWait::is_unique(&a1));

            let a2 = a1.clone();
            assert!(!ArcWait::is_unique(&a1));
            assert!(!ArcWait::is_unique(&a2));

            ArcWait::try_unwrap(a2).unwrap_err();

            assert!(ArcWait::is_unique(&a1));

            // should return immediately
            ArcWait::blocking_wait_unique(&mut a1);
            #[cfg(feature = "tokio_rt")]
            crate::sync::test::block_on(ArcWait::wait_unique(&mut a1));

            ArcWait::try_unwrap(a1).unwrap();
        });
    }

    #[rstest]
    #[timeout(secs(60))]
    pub fn drop_race_2_threads() {
        model(|| {
            // loom's Arc checks for memory leaks, so by storing it here
            // we get this for free
            let a1 = ArcWait::new(Arc::new(123));

            let a2 = a1.clone();
            thread::spawn(|| drop(a2));
        });
    }

    #[rstest]
    #[timeout(secs(60))]
    pub fn drop_race_3_threads() {
        model(|| {
            let a1 = ArcWait::new(Arc::new(123));

            let a2 = a1.clone();
            thread::spawn(|| drop(a2));

            let a3 = a1.clone();
            thread::spawn(|| drop(a3));
        });
    }

    #[rstest]
    #[timeout(secs(60))]
    pub fn blocking_wait_2_threads() {
        model(|| {
            let mut a1 = ArcWait::new(Arc::new(123));

            let a2 = a1.clone();
            thread::spawn(|| drop(a2));

            // ensure the clone is not optimized out
            let _ = std::hint::black_box(a1.clone());

            ArcWait::blocking_wait_unique(&mut a1);
            ArcWait::try_unwrap(a1).unwrap();
        });
    }

    #[rstest]
    #[timeout(secs(60))]
    #[cfg(any(feature = "tokio_rt", loom))]
    pub fn async_wait_2_threads() {
        model(|| {
            let mut a1 = ArcWait::new(Arc::new(123));

            let a2 = a1.clone();
            thread::spawn(|| drop(a2));

            // ensure the clone is not optimized out
            let _ = std::hint::black_box(a1.clone());

            crate::sync::test::block_on(ArcWait::wait_unique(&mut a1));
            ArcWait::try_unwrap(a1).unwrap();
        });
    }

    #[rstest]
    #[timeout(secs(60))]
    pub fn blocking_wait_3_threads() {
        model(|| {
            let mut a1 = ArcWait::new(Arc::new(123));

            let a2 = a1.clone();
            thread::spawn(|| drop(a2));

            let a3 = a1.clone();
            thread::spawn(|| drop(a3));

            ArcWait::blocking_wait_unique(&mut a1);
            ArcWait::try_unwrap(a1).unwrap();
        });
    }

    #[rstest]
    #[timeout(secs(60))]
    pub fn cancelled_wait_non_unique() {
        model(|| {
            let mut a1 = ArcWait::new(Arc::new(123));

            let _a2 = a1.clone();

            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            let mut fut = Box::pin(ArcWait::wait_unique(&mut a1));

            assert!(fut.as_mut().poll(&mut cx).is_pending());
            drop(fut);
        })
    }

    #[rstest]
    #[timeout(secs(60))]
    pub fn cancelled_wait_unique() {
        model(|| {
            let mut a1 = ArcWait::new(Arc::new(123));

            let _a2 = a1.clone();

            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            let mut fut = Box::pin(ArcWait::wait_unique(&mut a1));

            assert!(fut.as_mut().poll(&mut cx).is_pending());
            drop(fut);

            ArcWait::try_unwrap(a1).unwrap_err();
        })
    }

    #[rstest]
    #[timeout(secs(60))]
    pub fn cancelled_wait_2_threads() {
        model(|| {
            let mut a1 = ArcWait::new(Arc::new(123));

            let a2 = a1.clone();
            thread::spawn(|| drop(a2));

            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            let mut fut = Box::pin(ArcWait::wait_unique(&mut a1));

            // not interested in cancelling the future when ready, just after registration
            if fut.as_mut().poll(&mut cx).is_ready() {
                return;
            }
            drop(fut);

            // after this we can drop another clone and have them race
            let _ = std::hint::black_box(a1.clone());
            ArcWait::blocking_wait_unwrap(a1);
        })
    }

    #[rstest]
    #[timeout(secs(60))]
    pub fn forgotten_wait() {
        model(|| {
            let a1 = ArcWait::new(Arc::new(123));

            let a2 = a1.clone();
            let t2 = thread::spawn(|| drop(a2));

            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);

            // We can't use ArcWait::wait_unique here because we need to be able
            // to forget the future without leaking memory (to avoid reports from
            // miri/ASAN), and that is not possible to do soundly with an !Unpin
            // future.
            let mut fut = WaitUnique {
                arc: &a1,
                state: WaitUniqueState::Reserving,
            };

            // not interested in cancelling the future when ready, just after registration
            if Pin::new(&mut fut).poll(&mut cx).is_ready() {
                return;
            }
            std::mem::forget(fut);

            // drop another clone to have both drops racing on uniqueness with WAITER_SET
            let _ = std::hint::black_box(a1.clone());

            let ptr = a1.inner.as_ptr();
            drop(a1); // should not free the ArcWaitInner

            // join the thread and manually clean up the forgotten allocation to avoid
            // leak reports from Miri/ASAN
            t2.join().unwrap();

            unsafe { drop(Box::from_raw(ptr)) };
        })
    }
}
