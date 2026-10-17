/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Stable opaque handles for storage which may be entered concurrently.
//!
//! Godot treats `GDExtensionClassInstancePtr` as an opaque token. For blocking storage we deliberately do not expose the allocation address
//! as that token: a callback can arrive concurrently with `free_instance`, before it has acquired a storage claim, so dereferencing a direct
//! pointer in order to acquire that claim would itself race deallocation. A never-reused numeric handle lets us reject stale callbacks without
//! touching freed memory.

use std::any::TypeId;
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use godot_ffi as sys;

const SHARD_COUNT: usize = 64;

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct InstanceHandle(NonZeroUsize);

impl InstanceHandle {
    pub(super) fn from_instance_ptr(
        instance_ptr: sys::GDExtensionClassInstancePtr,
    ) -> Option<Self> {
        NonZeroUsize::new(instance_ptr.addr()).map(Self)
    }

    pub(super) fn into_instance_ptr(self) -> sys::GDExtensionClassInstancePtr {
        // The resulting pointer is only an opaque ABI token. It must never be dereferenced or converted into a storage pointer.
        std::ptr::without_provenance_mut(self.0.get())
    }

    fn value(self) -> usize {
        self.0.get()
    }
}

#[derive(Copy, Clone)]
struct ErasedStoragePtr(NonNull<()>);

// SAFETY: this wrapper never grants access to the pointee by itself. The registry only recovers a typed pointer after matching its `TypeId`,
// and production callers acquire or close the storage lifetime claim while still holding the corresponding registry lock.
unsafe impl Send for ErasedStoragePtr {}
unsafe impl Sync for ErasedStoragePtr {}

impl ErasedStoragePtr {
    fn cast<T>(self) -> *mut T {
        self.0.cast::<T>().as_ptr()
    }
}

#[derive(Copy, Clone)]
struct RegistryEntry {
    type_id: TypeId,
    storage: ErasedStoragePtr,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) struct HandleSpaceExhausted;

impl fmt::Display for HandleSpaceExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("opaque instance handle space exhausted")
    }
}

struct InstanceRegistry {
    // The all-ones value is retained as an exhausted sentinel. We never wrap and therefore never reuse a handle, including on 32-bit targets.
    next_handle: AtomicUsize,
    shards: [RwLock<HashMap<InstanceHandle, RegistryEntry>>; SHARD_COUNT],
}

impl InstanceRegistry {
    fn new() -> Self {
        Self::with_next_handle(1)
    }

    fn with_next_handle(next_handle: usize) -> Self {
        Self {
            next_handle: AtomicUsize::new(next_handle),
            shards: std::array::from_fn(|_| RwLock::new(HashMap::new())),
        }
    }

    fn allocate_handle(&self) -> Result<InstanceHandle, HandleSpaceExhausted> {
        let mut current = self.next_handle.load(Ordering::Relaxed);

        loop {
            let Some(next) = current.checked_add(1) else {
                return Err(HandleSpaceExhausted);
            };
            let Some(handle) = NonZeroUsize::new(current) else {
                return Err(HandleSpaceExhausted);
            };

            match self.next_handle.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(InstanceHandle(handle)),
                Err(actual) => current = actual,
            }
        }
    }

    fn shard_index(handle: InstanceHandle) -> usize {
        // Strip the monotonically increasing low bits before selecting a shard. This keeps adjacent instance allocations distributed while
        // retaining deterministic lookup.
        let mut value = handle.value();
        value ^= value >> 17;
        value ^= value >> 9;
        value & (SHARD_COUNT - 1)
    }

    fn shard(&self, handle: InstanceHandle) -> &RwLock<HashMap<InstanceHandle, RegistryEntry>> {
        &self.shards[Self::shard_index(handle)]
    }

    fn read_shard(
        &self,
        handle: InstanceHandle,
    ) -> RwLockReadGuard<'_, HashMap<InstanceHandle, RegistryEntry>> {
        self.shard(handle)
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_shard(
        &self,
        handle: InstanceHandle,
    ) -> RwLockWriteGuard<'_, HashMap<InstanceHandle, RegistryEntry>> {
        self.shard(handle)
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn insert<T: 'static>(&self, storage: *mut T) -> Result<InstanceHandle, HandleSpaceExhausted> {
        let storage = NonNull::new(storage).expect("cannot register null instance storage");
        let handle = self.allocate_handle()?;
        let entry = RegistryEntry {
            type_id: TypeId::of::<T>(),
            storage: ErasedStoragePtr(storage.cast()),
        };

        let previous = self.write_shard(handle).insert(handle, entry);
        debug_assert!(
            previous.is_none(),
            "never-reused handle was already registered"
        );
        Ok(handle)
    }

    fn with_read<T: 'static, R>(
        &self,
        handle: InstanceHandle,
        callback: impl FnOnce(*mut T) -> R,
    ) -> Option<R> {
        let shard = self.read_shard(handle);
        let entry = shard.get(&handle)?;
        if entry.type_id != TypeId::of::<T>() {
            return None;
        }

        Some(callback(entry.storage.cast::<T>()))
    }

    fn with_write<T: 'static, R>(
        &self,
        handle: InstanceHandle,
        callback: impl FnOnce(*mut T) -> R,
    ) -> Option<R> {
        let shard = self.write_shard(handle);
        let entry = shard.get(&handle)?;
        if entry.type_id != TypeId::of::<T>() {
            return None;
        }

        Some(callback(entry.storage.cast::<T>()))
    }

    fn remove<T: 'static>(&self, handle: InstanceHandle, expected_storage: *mut T) -> bool {
        let mut shard = self.write_shard(handle);
        let Some(entry) = shard.get(&handle) else {
            return false;
        };

        let expected_storage =
            NonNull::new(expected_storage).expect("cannot unregister null instance storage");
        if entry.type_id != TypeId::of::<T>() || entry.storage.0 != expected_storage.cast() {
            return false;
        }

        shard.remove(&handle);
        true
    }

    #[cfg(test)]
    fn contains(&self, handle: InstanceHandle) -> bool {
        self.read_shard(handle).contains_key(&handle)
    }
}

static INSTANCE_REGISTRY: LazyLock<InstanceRegistry> = LazyLock::new(InstanceRegistry::new);

pub(super) fn register<T: 'static>(
    storage: *mut T,
) -> Result<InstanceHandle, HandleSpaceExhausted> {
    INSTANCE_REGISTRY.insert(storage)
}

pub(super) fn with_read<T: 'static, R>(
    handle: InstanceHandle,
    callback: impl FnOnce(*mut T) -> R,
) -> Option<R> {
    INSTANCE_REGISTRY.with_read(handle, callback)
}

pub(super) fn with_write<T: 'static, R>(
    handle: InstanceHandle,
    callback: impl FnOnce(*mut T) -> R,
) -> Option<R> {
    INSTANCE_REGISTRY.with_write(handle, callback)
}

pub(super) fn remove<T: 'static>(handle: InstanceHandle, expected_storage: *mut T) -> bool {
    INSTANCE_REGISTRY.remove(handle, expected_storage)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex, mpsc};
    use std::thread;

    use super::*;
    use crate::storage::instance_storage::AtomicClaimState;

    struct FakeStorage {
        claims: AtomicClaimState,
        drops: Arc<AtomicUsize>,
    }

    impl FakeStorage {
        fn boxed(drops: &Arc<AtomicUsize>) -> Box<Self> {
            Box::new(Self {
                claims: AtomicClaimState::new(),
                drops: Arc::clone(drops),
            })
        }
    }

    impl Drop for FakeStorage {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    unsafe fn drop_registered(
        registry: &InstanceRegistry,
        handle: InstanceHandle,
        storage: *mut FakeStorage,
    ) {
        assert!(registry.remove(handle, storage));
        // SAFETY: the test removes the only registry entry and calls this only after all claims have been released.
        drop(unsafe { Box::from_raw(storage) });
    }

    struct FakeClaim<'a> {
        registry: &'a InstanceRegistry,
        handle: InstanceHandle,
        storage: ErasedStoragePtr,
    }

    impl Drop for FakeClaim<'_> {
        fn drop(&mut self) {
            let storage = self.storage.cast::<FakeStorage>();
            // SAFETY: construction increments this exact registered storage's claim count.
            if unsafe { (*storage).claims.release() } {
                // SAFETY: this was the last claim and close admission is already set.
                unsafe { drop_registered(self.registry, self.handle, storage) };
            }
        }
    }

    fn try_fake_claim(
        registry: &InstanceRegistry,
        handle: InstanceHandle,
    ) -> Option<FakeClaim<'_>> {
        registry
            .with_read::<FakeStorage, _>(handle, |storage| {
                // SAFETY: protected by the registry read lock.
                unsafe { (*storage).claims.try_claim() }.then(|| FakeClaim {
                    registry,
                    handle,
                    storage: ErasedStoragePtr(NonNull::new(storage.cast()).unwrap()),
                })
            })
            .flatten()
    }

    #[test]
    fn stale_handle_is_rejected_and_handles_are_never_reused() {
        let registry = InstanceRegistry::new();
        let drops = Arc::new(AtomicUsize::new(0));

        let first = Box::into_raw(FakeStorage::boxed(&drops));
        let first_handle = registry.insert(first).unwrap();
        assert!(
            registry
                .with_read::<FakeStorage, _>(first_handle, |_| ())
                .is_some()
        );
        unsafe { drop_registered(&registry, first_handle, first) };
        assert!(
            registry
                .with_read::<FakeStorage, _>(first_handle, |_| ())
                .is_none()
        );

        let second = Box::into_raw(FakeStorage::boxed(&drops));
        let second_handle = registry.insert(second).unwrap();
        assert_ne!(first_handle, second_handle);
        assert!(
            registry
                .with_read::<FakeStorage, _>(first_handle, |_| ())
                .is_none()
        );
        assert!(
            registry
                .with_read::<FakeStorage, _>(second_handle, |_| ())
                .is_some()
        );
        unsafe { drop_registered(&registry, second_handle, second) };

        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn stale_handle_does_not_alias_same_address_after_reregistration() {
        let registry = InstanceRegistry::new();
        let mut value = Box::new(41usize);
        let storage = NonNull::from(value.as_mut()).as_ptr();

        let stale = registry.insert(storage).unwrap();
        assert!(registry.remove(stale, storage));

        // Register the exact same allocation again. This deterministically models an allocator reusing
        // an address after destruction, without relying on allocator behavior in the test.
        *value = 42;
        let current = registry.insert(storage).unwrap();
        assert_ne!(stale, current);
        assert!(registry.with_read::<usize, _>(stale, |_| ()).is_none());
        assert_eq!(
            registry.with_read::<usize, _>(current, |ptr| unsafe { *ptr }),
            Some(42)
        );

        assert!(registry.remove(current, storage));
        assert!(!registry.contains(stale));
        assert!(!registry.contains(current));
    }

    #[test]
    fn stale_callback_churn_never_recovers_a_reregistered_address() {
        const LIFETIMES: usize = 500;
        const READERS: usize = 4;

        let registry = InstanceRegistry::new();
        let mut allocation = Box::new(0usize);
        let storage = NonNull::from(allocation.as_mut()).as_ptr();
        let retired = Mutex::new(Vec::with_capacity(LIFETIMES));
        let stale_resolutions = AtomicUsize::new(0);
        let current_registered = Arc::new(Barrier::new(READERS + 1));
        let stale_sweep_finished = Arc::new(Barrier::new(READERS + 1));

        thread::scope(|scope| {
            let readers: Vec<_> = (0..READERS)
                .map(|_| {
                    let registry = &registry;
                    let retired = &retired;
                    let stale_resolutions = &stale_resolutions;
                    let current_registered = Arc::clone(&current_registered);
                    let stale_sweep_finished = Arc::clone(&stale_sweep_finished);
                    scope.spawn(move || {
                        for _ in 0..LIFETIMES {
                            // The writer has registered a new lifetime at the exact same address before releasing this barrier.
                            current_registered.wait();
                            let handles = retired.lock().unwrap().clone();
                            for handle in handles {
                                if registry.with_read::<usize, _>(handle, |_| ()).is_some() {
                                    stale_resolutions.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            stale_sweep_finished.wait();
                        }
                    })
                })
                .collect();

            for lifetime in 0..LIFETIMES {
                *allocation = lifetime;
                let handle = registry.insert(storage).unwrap();

                current_registered.wait();
                stale_sweep_finished.wait();

                assert!(registry.remove(handle, storage));
                retired.lock().unwrap().push(handle);
            }

            for reader in readers {
                reader.join().unwrap();
            }
        });

        assert_eq!(stale_resolutions.load(Ordering::SeqCst), 0);
        assert_eq!(retired.into_inner().unwrap().len(), LIFETIMES);
        assert!(registry.shards.iter().all(|shard| {
            shard
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        }));
    }

    #[test]
    fn parallel_handle_churn_never_duplicates_or_leaves_entries() {
        const THREADS: usize = 8;
        const INSERTIONS_PER_THREAD: usize = 1_000;

        let registry = InstanceRegistry::new();
        let handles = Mutex::new(Vec::with_capacity(THREADS * INSERTIONS_PER_THREAD));
        let start = Arc::new(Barrier::new(THREADS));

        thread::scope(|scope| {
            let workers: Vec<_> = (0..THREADS)
                .map(|thread_index| {
                    let start = Arc::clone(&start);
                    let registry = &registry;
                    let handles = &handles;
                    scope.spawn(move || {
                        start.wait();
                        for value in 0..INSERTIONS_PER_THREAD {
                            let storage = Box::into_raw(Box::new((thread_index, value)));
                            let handle = registry.insert(storage).unwrap();
                            handles.lock().unwrap().push(handle);
                            assert!(registry.remove(handle, storage));
                            // SAFETY: this thread inserted and removed the only registry entry for this allocation.
                            drop(unsafe { Box::from_raw(storage) });
                        }
                    })
                })
                .collect();

            for worker in workers {
                worker.join().unwrap();
            }
        });

        let handles = handles.into_inner().unwrap();
        let unique: HashSet<_> = handles.iter().copied().collect();
        assert_eq!(handles.len(), THREADS * INSERTIONS_PER_THREAD);
        assert_eq!(unique.len(), handles.len());
        assert!(registry.shards.iter().all(|shard| {
            shard
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        }));
    }

    #[test]
    fn lookup_claim_linearizes_before_concurrent_close() {
        let registry = InstanceRegistry::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let storage = Box::into_raw(FakeStorage::boxed(&drops));
        let handle = registry.insert(storage).unwrap();
        let entered_read = Arc::new(Barrier::new(2));
        let release_read = Arc::new(Barrier::new(2));
        let close_completed = AtomicBool::new(false);
        let (close_attempt_tx, close_attempt_rx) = mpsc::sync_channel(0);

        thread::scope(|scope| {
            let callback_entered_read = Arc::clone(&entered_read);
            let callback_release_read = Arc::clone(&release_read);
            let registry_ref = &registry;
            let callback = scope.spawn(move || {
                registry_ref
                    .with_read::<FakeStorage, _>(handle, |storage| {
                        callback_entered_read.wait();
                        callback_release_read.wait();
                        // SAFETY: the registry read lock prevents close/removal while this pointer is used.
                        unsafe { (*storage).claims.try_claim() }
                    })
                    .unwrap()
            });

            entered_read.wait();
            let close_completed_ref = &close_completed;
            let registry_ref = &registry;
            let closer = scope.spawn(move || {
                close_attempt_tx.send(()).unwrap();
                let snapshot = registry_ref
                    .with_write::<FakeStorage, _>(handle, |storage| {
                        // SAFETY: the registry write lock excludes claim lookup and the allocation is registered.
                        unsafe { (*storage).claims.begin_close() }
                    })
                    .unwrap();
                close_completed_ref.store(true, Ordering::SeqCst);
                snapshot
            });

            close_attempt_rx.recv().unwrap();
            assert!(!close_completed.load(Ordering::SeqCst));
            release_read.wait();
            assert!(callback.join().unwrap());
            let snapshot = closer.join().unwrap();
            assert!(snapshot.has_call_claims());
        });

        // Release Godot's claim, then the admitted callback claim.
        assert!(!unsafe { (*storage).claims.release() });
        assert!(unsafe { (*storage).claims.release() });
        unsafe { drop_registered(&registry, handle, storage) };
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn close_wins_before_lookup_and_late_claim_is_rejected() {
        let registry = InstanceRegistry::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let storage = Box::into_raw(FakeStorage::boxed(&drops));
        let handle = registry.insert(storage).unwrap();

        let snapshot = registry
            .with_write::<FakeStorage, _>(handle, |storage| {
                // SAFETY: protected by the registry write lock.
                unsafe { (*storage).claims.begin_close() }
            })
            .unwrap();
        assert!(!snapshot.has_call_claims());
        let admitted = registry
            .with_read::<FakeStorage, _>(handle, |storage| {
                // SAFETY: protected by the registry read lock.
                unsafe { (*storage).claims.try_claim() }
            })
            .unwrap();
        assert!(!admitted);

        assert!(unsafe { (*storage).claims.release() });
        unsafe { drop_registered(&registry, handle, storage) };
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn entry_survives_close_for_reentrant_weak_lookup_until_last_claim() {
        let registry = InstanceRegistry::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let storage = Box::into_raw(FakeStorage::boxed(&drops));
        let handle = registry.insert(storage).unwrap();

        assert!(unsafe { (*storage).claims.try_claim() });
        registry
            .with_write::<FakeStorage, _>(handle, |storage| {
                // SAFETY: protected by the registry write lock.
                unsafe { (*storage).claims.begin_close() }
            })
            .unwrap();
        assert!(!unsafe { (*storage).claims.release() }); // Godot claim.

        // A synchronous reentrant path can still resolve the storage because the outer callback claim pins it.
        assert_eq!(
            registry
                .with_read::<FakeStorage, _>(handle, |resolved| resolved)
                .unwrap(),
            storage
        );
        assert!(registry.contains(handle));

        assert!(unsafe { (*storage).claims.release() });
        unsafe { drop_registered(&registry, handle, storage) };
        assert!(!registry.contains(handle));
        assert!(
            registry
                .with_read::<FakeStorage, _>(handle, |_| ())
                .is_none()
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn last_claim_removes_entry_before_deallocation() {
        let registry = InstanceRegistry::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let storage = Box::into_raw(FakeStorage::boxed(&drops));
        let handle = registry.insert(storage).unwrap();

        registry
            .with_write::<FakeStorage, _>(handle, |storage| {
                // SAFETY: protected by the registry write lock.
                unsafe { (*storage).claims.begin_close() }
            })
            .unwrap();
        assert!(unsafe { (*storage).claims.release() });

        assert!(registry.remove(handle, storage));
        assert!(!registry.contains(handle));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        // SAFETY: the entry is gone and the last claim was released.
        drop(unsafe { Box::from_raw(storage) });
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn handle_counter_exhaustion_fails_closed_without_wrap_or_insertion() {
        let registry = InstanceRegistry::with_next_handle(usize::MAX);
        let drops = Arc::new(AtomicUsize::new(0));
        let mut storage = FakeStorage::boxed(&drops);
        let storage_ptr = NonNull::from(storage.as_mut()).as_ptr();

        assert_eq!(registry.insert(storage_ptr), Err(HandleSpaceExhausted));
        assert_eq!(registry.next_handle.load(Ordering::Relaxed), usize::MAX);
        assert!(registry.shards.iter().all(|shard| {
            shard
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        }));
        drop(storage);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn handle_pointer_round_trip_preserves_only_numeric_identity() {
        let handle = InstanceHandle(NonZeroUsize::new(37).unwrap());
        let encoded = handle.into_instance_ptr();
        assert_eq!(InstanceHandle::from_instance_ptr(encoded), Some(handle));
        assert_eq!(encoded.addr(), 37);
    }

    #[test]
    fn wrong_type_lookup_and_remove_do_not_touch_entry() {
        let registry = InstanceRegistry::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let storage = Box::into_raw(FakeStorage::boxed(&drops));
        let handle = registry.insert(storage).unwrap();

        assert!(registry.with_read::<u8, _>(handle, |_| ()).is_none());
        assert!(!registry.remove::<u8>(handle, storage.cast()));
        assert!(registry.contains(handle));

        unsafe { drop_registered(&registry, handle, storage) };
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn remove_waits_for_active_reader_before_deallocation() {
        let registry = InstanceRegistry::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let storage = Box::into_raw(FakeStorage::boxed(&drops));
        let handle = registry.insert(storage).unwrap();
        let (reader_entered_tx, reader_entered_rx) = mpsc::sync_channel(0);
        let (reader_release_tx, reader_release_rx) = mpsc::sync_channel(0);
        let (remove_attempt_tx, remove_attempt_rx) = mpsc::sync_channel(0);

        thread::scope(|scope| {
            let registry_ref = &registry;
            let reader = scope.spawn(move || {
                registry_ref.with_read::<FakeStorage, _>(handle, |_| {
                    reader_entered_tx.send(()).unwrap();
                    reader_release_rx.recv().unwrap();
                })
            });

            reader_entered_rx.recv().unwrap();
            let registry_ref = &registry;
            let storage_send = ErasedStoragePtr(NonNull::new(storage.cast()).unwrap());
            let remover = scope.spawn(move || {
                remove_attempt_tx.send(()).unwrap();
                registry_ref.remove(handle, storage_send.cast::<FakeStorage>())
            });
            remove_attempt_rx.recv().unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            reader_release_tx.send(()).unwrap();
            assert_eq!(reader.join().unwrap(), Some(()));
            assert!(remover.join().unwrap());
        });

        // SAFETY: removal waited for the reader and no entry/claim remains in this focused lock test.
        drop(unsafe { Box::from_raw(storage) });
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_claim_stress_drops_once_and_leaves_registry_empty() {
        const THREADS: usize = 8;
        const ATTEMPTS: usize = 1_000;

        let registry = InstanceRegistry::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let storage = Box::into_raw(FakeStorage::boxed(&drops));
        let handle = registry.insert(storage).unwrap();
        let start = Arc::new(Barrier::new(THREADS + 1));

        thread::scope(|scope| {
            let mut workers = Vec::new();
            for _ in 0..THREADS {
                let start = Arc::clone(&start);
                let registry_ref = &registry;
                workers.push(scope.spawn(move || {
                    start.wait();
                    for _ in 0..ATTEMPTS {
                        let Some(claim) = try_fake_claim(registry_ref, handle) else {
                            break;
                        };
                        drop(claim);
                        thread::yield_now();
                    }
                }));
            }

            start.wait();
            let _snapshot = registry
                .with_write::<FakeStorage, _>(handle, |storage| {
                    // SAFETY: the registry write lock excludes claim admission.
                    unsafe { (*storage).claims.begin_close() }
                })
                .unwrap();

            // SAFETY: release the distinguished Godot claim exactly once. Either this path or the final worker claim owns deallocation.
            if unsafe { (*storage).claims.release() } {
                // SAFETY: no worker claim remains.
                unsafe { drop_registered(&registry, handle, storage) };
            }

            for worker in workers {
                worker.join().unwrap();
            }
        });

        assert!(!registry.contains(handle));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            registry
                .with_read::<FakeStorage, _>(handle, |_| ())
                .is_none()
        );
    }
}
