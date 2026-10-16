/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.
 */

use std::mem::ManuallyDrop;
use std::thread::ThreadId;

use godot_ffi as sys;

/// An explicitly unsafe ownership transfer whose value may only be accessed on Godot's main thread.
///
/// Some Godot APIs copy dynamically typed values through an engine-owned worker queue and later require an extension to use those values on
/// the main thread. Rust deliberately does not implement [`Send`] for types such as [`Callable`][crate::builtin::Callable],
/// [`Variant`][crate::builtin::Variant], or [`Gd`][crate::obj::Gd], because arbitrary instances of those types are not generally safe to move
/// between threads. `MainThreadTransfer` provides a narrow escape hatch for an API-specific guarantee that the type system cannot express.
///
/// The wrapper can cross threads, but it never exposes or destroys `T` away from the target main thread. Dropping it on another thread leaks
/// `T`; this is preferable to running a potentially thread-affine destructor on the wrong thread. Use [`try_into_inner()`][Self::try_into_inner]
/// to retain ownership when the current thread is uncertain.
///
/// Creating this wrapper is unsafe. It does not make a value thread-safe, does not synchronize aliases, and does not permit calling methods on
/// the value from a worker thread.
pub struct MainThreadTransfer<T> {
    value: ManuallyDrop<T>,
    target_thread: ThreadId,
    taken: bool,
}

// SAFETY: public accessors only expose `T` on `target_thread`. Drop also destroys `T` only on that thread; elsewhere it deliberately leaks it.
// The unsafe constructor makes the caller prove that transferring the dormant value itself and subsequently using it on the main thread is
// valid for this particular `T` and engine API.
unsafe impl<T> Send for MainThreadTransfer<T> {}

impl<T> MainThreadTransfer<T> {
    /// Wraps `value` for ownership transfer to Godot's main thread.
    ///
    /// # Safety
    ///
    /// The caller must prove all of the following for the concrete value and API:
    ///
    /// - moving the dormant value through other threads is valid;
    /// - the value will only be accessed or intentionally destroyed on Godot's main thread;
    /// - aliases on other threads cannot concurrently mutate, access, or destroy thread-unsafe state reachable from the value;
    /// - every access and destruction happens while the Godot binding and any engine facilities needed by `T` are still initialized;
    /// - leaking the value is safe if the transfer is abandoned on a worker thread.
    ///
    /// Godot having copied a value through an internal queue is useful evidence, but is not by itself a blanket guarantee for every operation
    /// on that value.
    pub unsafe fn new_for_main_thread(value: T) -> Self {
        #[cfg(not(wasm_nothreads))]
        let main_thread = sys::main_thread_id();

        // A non-threaded WebAssembly build has exactly one execution thread, which is also Godot's main thread.
        #[cfg(wasm_nothreads)]
        let main_thread = sys::current_thread_id();

        // SAFETY: the caller establishes the transfer contract documented above; this helper only records the destination.
        unsafe { Self::new_for_thread(value, main_thread) }
    }

    /// Returns whether access is currently permitted.
    pub fn is_on_main_thread(&self) -> bool {
        self.is_on_target_thread()
    }

    /// Borrows the value on Godot's main thread.
    ///
    /// # Panics
    ///
    /// Panics if called from another thread.
    pub fn get(&self) -> &T {
        assert!(
            self.is_on_target_thread(),
            "MainThreadTransfer<T> can only be accessed on Godot's main thread"
        );

        // SAFETY: access is restricted to the target thread and `taken` can only become true by consuming `self`.
        unsafe { &*(&self.value as *const ManuallyDrop<T>).cast::<T>() }
    }

    /// Mutably borrows the value on Godot's main thread.
    ///
    /// # Panics
    ///
    /// Panics if called from another thread.
    pub fn get_mut(&mut self) -> &mut T {
        assert!(
            self.is_on_target_thread(),
            "MainThreadTransfer<T> can only be accessed on Godot's main thread"
        );

        // SAFETY: `&mut self` is exclusive, access is on the target thread, and a live wrapper has not been consumed.
        unsafe { &mut *(&mut self.value as *mut ManuallyDrop<T>).cast::<T>() }
    }

    /// Consumes the wrapper on Godot's main thread.
    ///
    /// If called from another thread, returns the intact wrapper without accessing the inner value.
    pub fn try_into_inner(mut self) -> Result<T, Self> {
        if !self.is_on_target_thread() {
            return Err(self);
        }

        self.taken = true;
        // SAFETY: access is on the target thread and `taken` prevents Drop from destroying the value twice.
        Ok(unsafe { ManuallyDrop::take(&mut self.value) })
    }

    /// Consumes the wrapper and returns its value on Godot's main thread.
    ///
    /// # Panics
    ///
    /// Panics if called from another thread. During unwinding, the wrapped value is leaked rather than destroyed there.
    pub fn into_inner(self) -> T {
        match self.try_into_inner() {
            Ok(value) => value,
            Err(_transfer) => {
                panic!("MainThreadTransfer<T> can only be consumed on Godot's main thread")
            }
        }
    }

    unsafe fn new_for_thread(value: T, target_thread: ThreadId) -> Self {
        Self {
            value: ManuallyDrop::new(value),
            target_thread,
            taken: false,
        }
    }

    fn is_on_target_thread(&self) -> bool {
        self.target_thread == sys::current_thread_id()
    }
}

impl<T> Drop for MainThreadTransfer<T> {
    fn drop(&mut self) {
        if self.taken || !self.is_on_target_thread() {
            return;
        }

        // SAFETY: this is the recorded target thread and the value has not been taken.
        unsafe { ManuallyDrop::drop(&mut self.value) };
        self.taken = true;
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::MainThreadTransfer;

    struct DropCounter(Arc<AtomicUsize>);

    fn assert_send<T: Send>() {}

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn transfer_can_return_to_target_thread() {
        assert_send::<MainThreadTransfer<Rc<()>>>();

        let target = std::thread::current().id();
        let transfer = unsafe { MainThreadTransfer::new_for_thread(String::from("value"), target) };

        let transfer = std::thread::spawn(move || {
            assert!(!transfer.is_on_target_thread());
            transfer
        })
        .join()
        .unwrap();

        assert_eq!(transfer.into_inner(), "value");
    }

    #[test]
    fn wrong_thread_drop_leaks_instead_of_running_destructor() {
        let drops = Arc::new(AtomicUsize::new(0));
        let target = std::thread::current().id();
        let transfer =
            unsafe { MainThreadTransfer::new_for_thread(DropCounter(drops.clone()), target) };

        std::thread::spawn(move || drop(transfer)).join().unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn target_thread_drop_runs_destructor_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let target = std::thread::current().id();
        let transfer =
            unsafe { MainThreadTransfer::new_for_thread(DropCounter(drops.clone()), target) };

        drop(transfer);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}
