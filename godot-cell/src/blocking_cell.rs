/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::error::Error;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
#[cfg(test)]
use std::thread;

use crate::blocking_guards::{InaccessibleGuardBlocking, MutGuardBlocking, RefGuardBlocking};
use crate::cell::GdCellInner;

/// Blocking version of [`panicking::GdCell`](crate::panicking::GdCell) for multithreaded usage.
///
/// This version of GdCell blocks the current thread if it does not yet hold references to the cell.
/// Since `GdCellInner` isn't thread-safe by itself, any access to `inner` must be guarded by locking the `thread_tracker`.
///
/// For more details on when threads are being blocked see [`Self::borrow`] and [`Self::borrow_mut`].
///
/// See [`panicking::GdCell`](crate::panicking::GdCell) for more details on the base concept of this type.
pub struct GdCellBlocking<T> {
    inner: Pin<Box<GdCellInner<T>>>,
    thread_tracker: Arc<Mutex<ThreadTracker>>,
    immut_condition: Arc<Condvar>,
    mut_condition: Arc<Condvar>,
}

impl<T> GdCellBlocking<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: GdCellInner::new(value),
            thread_tracker: Arc::default(),
            immut_condition: Arc::new(Condvar::new()),
            mut_condition: Arc::new(Condvar::new()),
        }
    }

    /// Returns a new shared reference to the contents of the cell.
    ///
    /// Fails if an accessible mutable reference exists on the current thread.
    ///
    /// Blocks if another thread currently holds any reference.
    pub fn borrow(&self) -> Result<RefGuardBlocking<'_, T>, Box<dyn Error>> {
        let mut tracker_guard = self.thread_tracker.lock().unwrap();

        if self.current_thread_must_wait(&mut tracker_guard) {
            // Block current thread until borrow becomes available.
            tracker_guard = self.block_immut(tracker_guard);
        }

        let should_claim_mut = !self.is_currently_bound_locked();

        let inner_guard = self.inner.as_ref().borrow()?;

        tracker_guard.increment_current_thread_shared_count();

        // The ThreadTracker::mut_thread is always set to some ThreadId to avoid additional tracking overhead, but this causes an edge case:
        // 1. mut_thread is initialized with the current Thread 1 (usually main thread).
        // 2. Thread 2 acquires an immutable borrow from the cell.
        // 3. Thread 1 attempts to acquire a mutable borrow from the cell.
        // 4. No immutable borrow exists on Thread 1, but the thread is assigned as the mut_thread; no blocking occurs.
        // 5. The mutable borrow fails and panics because there is already an immutable borrow on Thread 1.
        //
        // Solution:
        // 1. Always reassign mut_thread to the current ThreadId (i.e. Thread 2) when the first immutable borrow is acquired.
        // 2. Thread 1 will now block on mutable borrows because it is not the mut_thread.
        // 3. Thread 2 should never block, as it already holds an immutable borrow.
        if should_claim_mut {
            tracker_guard.claim_mut_ref();
        }

        Ok(RefGuardBlocking::new(
            inner_guard,
            self.mut_condition.clone(),
            self.thread_tracker.clone(),
        ))
    }

    /// Returns a new mutable reference to the contents of the cell.
    ///
    /// Fails if an accessible mutable reference, or a shared reference exists on the current thread.
    ///
    /// Blocks if another thread currently holds a mutable reference, or if another thread holds immutable references but the current thread
    /// doesn't.
    pub fn borrow_mut(&self) -> Result<MutGuardBlocking<'_, T>, Box<dyn Error>> {
        let mut tracker_guard = self.thread_tracker.lock().unwrap();

        if self.current_thread_must_wait(&mut tracker_guard) {
            // Block current thread until borrow becomes available.
            tracker_guard = self.block_mut(tracker_guard);
        }

        let inner_guard = self.inner.as_ref().borrow_mut()?;

        tracker_guard.claim_mut_ref();

        Ok(MutGuardBlocking::new(
            inner_guard,
            self.mut_condition.clone(),
            self.immut_condition.clone(),
            self.thread_tracker.clone(),
        ))
    }

    /// Make the current mutable borrow inaccessible, thus freeing the value up to be reborrowed again.
    ///
    /// Will error if:
    /// - There is currently no accessible mutable borrow.
    /// - There are any shared references.
    /// - `current_ref` is not equal to the pointer in `self.inner.state`.
    pub fn make_inaccessible<'cell, 'val>(
        &'cell self,
        current_ref: &'val mut T,
    ) -> Result<InaccessibleGuardBlocking<'val, T>, Box<dyn Error>>
    where
        'cell: 'val,
    {
        let _tracker_guard = self.thread_tracker.lock().unwrap();
        let inner = self.inner.as_ref().make_inaccessible(current_ref)?;
        let inaccessible = InaccessibleGuardBlocking::new(inner, self.thread_tracker.clone());
        Ok(inaccessible)
    }

    /// Returns `true` if there are any mutable or shared references, regardless of whether the mutable
    /// references are accessible or not.
    ///
    /// In particular this means that it is safe to destroy this cell and the value contained within, as no
    /// references can exist that can reference this cell.
    ///
    /// Keep in mind that in multithreaded code it is still possible for this to return true, and then the
    /// cell hands out a new borrow before it is destroyed. So we still need to ensure that this cannot
    /// happen at the same time.
    pub fn is_currently_bound(&self) -> bool {
        let _tracker_guard = self.thread_tracker.lock().unwrap();
        self.is_currently_bound_locked()
    }

    /// Reads the inner borrow state while the caller holds `thread_tracker`.
    fn is_currently_bound_locked(&self) -> bool {
        self.inner.as_ref().is_currently_bound()
    }

    /// Returns whether the current thread must wait for the thread that owns the active borrows.
    fn current_thread_must_wait(&self, tracker: &mut ThreadTracker) -> bool {
        self.is_currently_bound_locked()
            && tracker.current_thread_shared_count() == 0
            && !tracker.current_thread_has_mut_ref()
    }

    /// Blocks the current thread until all mutable and shared references have been dropped.
    fn block_mut<'a>(
        &self,
        mut tracker_guard: MutexGuard<'a, ThreadTracker>,
    ) -> MutexGuard<'a, ThreadTracker> {
        while self.is_currently_bound_locked() {
            tracker_guard = self.mut_condition.wait(tracker_guard).unwrap();
        }

        tracker_guard
    }

    /// Blocks the current thread until all references have been dropped.
    fn block_immut<'a>(
        &self,
        mut tracker_guard: MutexGuard<'a, ThreadTracker>,
    ) -> MutexGuard<'a, ThreadTracker> {
        while self.inner.as_ref().is_currently_mutably_bound() {
            tracker_guard = self.immut_condition.wait(tracker_guard).unwrap();
        }

        while self.is_currently_bound_locked() {
            tracker_guard = self.mut_condition.wait(tracker_guard).unwrap();
        }

        tracker_guard
    }
}

// SAFETY:
// - The only way to access the underlying `T` is via `GdCellBlocking`.
// - The `thread_tracker` lock serializes guard creation and allows guards to coexist only on the thread
//   that owns the active borrows. Another thread waits until all those guards have been dropped.
// Therefore `T` need only be `Send`: access can move between threads, but cannot happen concurrently.
unsafe impl<T: Send> Sync for GdCellBlocking<T> {}

/// Holds the reference count and the currently mutable thread.
#[derive(Debug)]
pub(crate) struct ThreadTracker {
    /// Thread ID of the thread that currently can hold the mutable reference.
    ///
    /// This is not an Option, contrary to what one might expect. Making this an option would require reliable knowledge that not a single
    /// MutGuardBlocking exists before setting it to None. This would require tracking a count of mutable borrows. Instead, we always set
    /// the field to an acceptable value and avoid the overhead.
    mut_thread: usize,

    /// Shared-reference count per thread.
    ///
    /// `Vec` instead of `HashMap`: #threads concurrently holding a shared borrow on a cell is almost always 0 or 1, so linear search is cheaper.
    shared_counts: Vec<(usize, usize)>,
}

impl Default for ThreadTracker {
    fn default() -> Self {
        Self {
            mut_thread: current_thread_id(),
            shared_counts: Vec::new(),
        }
    }
}

/// Destructor-free thread identity, duplicated here to keep this crate dependency-free.
///
/// `std::thread::current()` installs Rust runtime thread cleanup on foreign worker threads. That
/// cleanup can outlive a hot-reloaded cdylib on Linux. A const no-drop TLS marker has a stable,
/// unique address for each live thread without registering a destructor.
fn current_thread_id() -> usize {
    thread_local! {
        static CURRENT_THREAD_MARKER: u8 = const { 0 };
    }

    CURRENT_THREAD_MARKER.with(|marker| marker as *const u8 as usize)
}

impl ThreadTracker {
    /// Number of shared references in the current thread.
    // Could be &self, but not needed and this allows to reuse find_shared_count().
    pub fn current_thread_shared_count(&mut self) -> usize {
        self.find_shared_count(current_thread_id())
            .map_or(0, |count| *count)
    }

    /// Increments the shared reference count in the current thread.
    pub fn increment_current_thread_shared_count(&mut self) {
        let thread_id = current_thread_id();
        match self.find_shared_count(thread_id) {
            Some(count) => *count += 1,
            None => self.shared_counts.push((thread_id, 1)),
        }
    }

    /// Decrements the shared reference count in the current thread.
    pub fn decrement_current_thread_shared_count(&mut self) {
        let thread_id = current_thread_id();
        let entry = self
            .shared_counts
            .iter()
            .position(|(id, _)| *id == thread_id);

        debug_assert!(
            entry.is_some(),
            "No shared reference count exists for {thread_id:?}."
        );

        let Some(index) = entry else {
            return;
        };

        let count = self.shared_counts[index].1;
        debug_assert!(count > 0, "Shared reference count is already zero.");

        if count > 1 {
            self.shared_counts[index].1 = count - 1;
        } else {
            // Worker pools are usually bounded, but the cell must not retain every transient
            // ThreadId which has ever borrowed it. A later borrow from this thread simply
            // recreates the entry.
            self.shared_counts.swap_remove(index);
        }
    }

    /// Returns if the current thread can hold the mutable reference.
    pub fn current_thread_has_mut_ref(&self) -> bool {
        self.mut_thread == current_thread_id()
    }

    fn find_shared_count(&mut self, thread_id: usize) -> Option<&mut usize> {
        self.shared_counts
            .iter_mut()
            .find(|(id, _)| *id == thread_id)
            .map(|(_, count)| count)
    }

    /// Claims the mutable reference for the current thread.
    fn claim_mut_ref(&mut self) {
        self.mut_thread = current_thread_id();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn shared_borrows_are_reentrant_on_the_same_thread() {
        let cell = GdCellBlocking::new(Cell::new(1));

        let first = cell.borrow().unwrap();
        let second = cell.borrow().unwrap();

        first.set(2);
        assert_eq!(second.get(), 2);
    }

    #[test]
    fn shared_borrow_from_another_thread_must_wait() {
        let cell = GdCellBlocking::new(Cell::new(1));
        let _guard = cell.borrow().unwrap();

        thread::scope(|scope| {
            let must_wait = scope
                .spawn(|| {
                    let mut tracker = cell.thread_tracker.lock().unwrap();
                    cell.current_thread_must_wait(&mut tracker)
                })
                .join()
                .unwrap();

            assert!(must_wait);
        });
    }

    #[test]
    fn shared_borrow_transfers_to_another_thread_after_release() {
        let cell = GdCellBlocking::new(Cell::new(1));
        let guard = cell.borrow().unwrap();

        thread::scope(|scope| {
            let (started_tx, started_rx) = mpsc::channel();
            let (value_tx, value_rx) = mpsc::channel();
            let cell = &cell;

            let worker = scope.spawn(move || {
                started_tx.send(()).unwrap();
                let guard = cell.borrow().unwrap();
                value_tx.send(guard.get()).unwrap();
            });

            started_rx.recv().unwrap();
            drop(guard);

            assert_eq!(value_rx.recv().unwrap(), 1);
            worker.join().unwrap();
        });
    }

    #[test]
    fn completed_threads_do_not_accumulate_shared_count_entries() {
        let cell = GdCellBlocking::new(Cell::new(1));

        thread::scope(|scope| {
            for _ in 0..64 {
                scope
                    .spawn(|| {
                        let guard = cell.borrow().unwrap();
                        assert_eq!(guard.get(), 1);
                    })
                    .join()
                    .unwrap();
            }
        });

        assert!(cell.thread_tracker.lock().unwrap().shared_counts.is_empty());
    }

    #[test]
    fn cross_thread_shared_borrow_really_blocks_until_release() {
        let cell = GdCellBlocking::new(Cell::new(1));
        let guard = cell.borrow().unwrap();

        thread::scope(|scope| {
            let (started_tx, started_rx) = mpsc::channel();
            let (acquired_tx, acquired_rx) = mpsc::channel();
            let cell = &cell;

            let worker = scope.spawn(move || {
                started_tx.send(()).unwrap();
                let worker_guard = cell.borrow().unwrap();
                acquired_tx.send(worker_guard.get()).unwrap();
            });

            started_rx.recv().unwrap();
            assert!(matches!(
                acquired_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));

            drop(guard);
            assert_eq!(acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
            worker.join().unwrap();
        });
    }

    #[test]
    fn cross_thread_mutable_borrow_waits_for_shared_release() {
        let cell = GdCellBlocking::new(Cell::new(1));
        let guard = cell.borrow().unwrap();

        thread::scope(|scope| {
            let (started_tx, started_rx) = mpsc::channel();
            let (acquired_tx, acquired_rx) = mpsc::channel();
            let cell = &cell;

            let worker = scope.spawn(move || {
                started_tx.send(()).unwrap();
                let worker_guard = cell.borrow_mut().unwrap();
                acquired_tx.send(worker_guard.get()).unwrap();
            });

            started_rx.recv().unwrap();
            assert!(matches!(
                acquired_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));

            drop(guard);
            assert_eq!(acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
            worker.join().unwrap();
        });
    }

    #[test]
    fn cross_thread_shared_borrow_waits_for_mutable_release() {
        let cell = GdCellBlocking::new(Cell::new(1));
        let guard = cell.borrow_mut().unwrap();

        thread::scope(|scope| {
            let (started_tx, started_rx) = mpsc::channel();
            let (acquired_tx, acquired_rx) = mpsc::channel();
            let cell = &cell;

            let worker = scope.spawn(move || {
                started_tx.send(()).unwrap();
                let worker_guard = cell.borrow().unwrap();
                acquired_tx.send(worker_guard.get()).unwrap();
            });

            started_rx.recv().unwrap();
            assert!(matches!(
                acquired_rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));

            drop(guard);
            assert_eq!(acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
            worker.join().unwrap();
        });
    }

    #[test]
    fn panicking_borrower_does_not_strand_waiters() {
        let cell = GdCellBlocking::new(Cell::new(1));

        thread::scope(|scope| {
            let cell = &cell;
            let panicking_worker = scope.spawn(move || {
                let _guard = cell.borrow().unwrap();
                panic!("intentional panic while borrowed");
            });
            assert!(panicking_worker.join().is_err());

            let (acquired_tx, acquired_rx) = mpsc::channel();
            let next_worker = scope.spawn(move || {
                let guard = cell.borrow().unwrap();
                acquired_tx.send(guard.get()).unwrap();
            });

            assert_eq!(acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
            next_worker.join().unwrap();
        });
    }

    #[test]
    fn non_sync_value_is_never_observed_by_two_threads_at_once() {
        struct Probe {
            active: AtomicUsize,
            visits: Cell<usize>,
        }

        impl Probe {
            fn visit(&self) {
                assert_eq!(self.active.fetch_add(1, Ordering::SeqCst), 0);
                thread::yield_now();
                self.visits.set(self.visits.get() + 1);
                assert_eq!(self.active.fetch_sub(1, Ordering::SeqCst), 1);
            }
        }

        let cell = GdCellBlocking::new(Probe {
            active: AtomicUsize::new(0),
            visits: Cell::new(0),
        });

        thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        for _ in 0..250 {
                            cell.borrow().unwrap().visit();
                        }
                    })
                })
                .collect();

            for worker in workers {
                worker.join().unwrap();
            }
        });

        assert_eq!(cell.borrow().unwrap().visits.get(), 2_000);
    }
}
