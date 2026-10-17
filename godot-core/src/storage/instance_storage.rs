/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::cell::Cell;
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use godot_cell::{blocking, panicking};
use godot_ffi as sys;

use crate::godot_error;
use crate::obj::{Base, ClassThreading, Gd, GodotClass, Inherits, Singleton};
use crate::storage::instance_registry::{self, InstanceHandle};
use crate::storage::log_pre_drop;

#[derive(Debug)]
pub enum RefGuard<'a, T> {
    Single(panicking::RefGuard<'a, T>),
    Blocking(blocking::RefGuard<'a, T>),
}

impl<T> Deref for RefGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Single(guard) => guard,
            Self::Blocking(guard) => guard,
        }
    }
}

#[derive(Debug)]
pub enum MutGuard<'a, T> {
    Single(panicking::MutGuard<'a, T>),
    Blocking(blocking::MutGuard<'a, T>),
}

impl<T> Deref for MutGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Single(guard) => guard,
            Self::Blocking(guard) => guard,
        }
    }
}

impl<T> DerefMut for MutGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Single(guard) => guard,
            Self::Blocking(guard) => guard,
        }
    }
}

#[derive(Debug)]
pub enum InaccessibleGuard<'a, T> {
    Single(panicking::InaccessibleGuard<'a, T>),
    Blocking(blocking::InaccessibleGuard<'a, T>),
}

sys::atomic_enum! {
    #[derive(Copy, Clone, Debug)]
    pub enum Lifecycle {
        Alive = 0,
        Destroying = 1,
    }
}

/// Type-safe atomic wrapper for [`Lifecycle`].
#[cfg_attr(not(feature = "experimental-threads"), allow(dead_code))]
pub type AtomicLifecycle = sys::AtomicEnum<Lifecycle>;

const CLAIMS_CLOSED_BIT: u32 = 1 << 31;
const CLAIMS_COUNT_MASK: u32 = CLAIMS_CLOSED_BIT - 1;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) struct CloseSnapshot {
    claim_count: u32,
}

impl CloseSnapshot {
    pub(super) fn has_call_claims(self) -> bool {
        // One distinguished claim belongs to Godot from construction until the free callback.
        self.claim_count > 1
    }
}

/// Claim admission for single-threaded storage. It mirrors [`AtomicClaimState`] so destruction uses one protocol for both storage kinds.
pub(super) struct LocalClaimState {
    state: Cell<u32>,
}

impl LocalClaimState {
    pub(super) fn new() -> Self {
        Self {
            state: Cell::new(1),
        }
    }

    pub(super) fn try_claim(&self) -> bool {
        let state = self.state.get();
        let count = state & CLAIMS_COUNT_MASK;

        if state & CLAIMS_CLOSED_BIT != 0 || count == 0 || count == CLAIMS_COUNT_MASK {
            return false;
        }

        self.state.set(state + 1);
        true
    }

    pub(super) fn begin_close(&self) -> CloseSnapshot {
        let state = self.state.get();
        let count = state & CLAIMS_COUNT_MASK;

        assert_eq!(
            state & CLAIMS_CLOSED_BIT,
            0,
            "storage closed more than once"
        );
        assert_ne!(count, 0, "storage closed without an owning claim");

        self.state.set(state | CLAIMS_CLOSED_BIT);
        CloseSnapshot { claim_count: count }
    }

    pub(super) fn release(&self) -> bool {
        let state = self.state.get();
        let count = state & CLAIMS_COUNT_MASK;

        assert_ne!(count, 0, "claim released more often than taken");
        assert!(
            count != 1 || state & CLAIMS_CLOSED_BIT != 0,
            "last claim released before storage was closed"
        );

        self.state.set(state - 1);
        count == 1
    }
}

/// Packed close flag and lifetime-claim count for blocking storage.
///
/// `begin_close()` atomically forbids new claims while retaining Godot's distinguished claim. A successful `try_claim()` therefore either
/// linearizes before close and is counted until it releases, or observes close and fails; the counter can never be resurrected from zero.
pub(super) trait AtomicClaimWord {
    fn load(&self, order: Ordering) -> u32;
    fn fetch_or(&self, value: u32, order: Ordering) -> u32;
    fn compare_exchange_weak(
        &self,
        current: u32,
        new: u32,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u32, u32>;
}

impl AtomicClaimWord for AtomicU32 {
    fn load(&self, order: Ordering) -> u32 {
        self.load(order)
    }

    fn fetch_or(&self, value: u32, order: Ordering) -> u32 {
        self.fetch_or(value, order)
    }

    fn compare_exchange_weak(
        &self,
        current: u32,
        new: u32,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u32, u32> {
        self.compare_exchange_weak(current, new, success, failure)
    }
}

pub(super) struct AtomicClaimState<A = AtomicU32> {
    state: A,
}

impl AtomicClaimState {
    pub(super) fn new() -> Self {
        Self {
            state: AtomicU32::new(1),
        }
    }
}

impl<A: AtomicClaimWord> AtomicClaimState<A> {
    pub(super) fn try_claim(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);

        loop {
            let count = state & CLAIMS_COUNT_MASK;
            if state & CLAIMS_CLOSED_BIT != 0 || count == 0 || count == CLAIMS_COUNT_MASK {
                return false;
            }

            match self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => state = actual,
            }
        }
    }

    pub(super) fn begin_close(&self) -> CloseSnapshot {
        let state = self.state.fetch_or(CLAIMS_CLOSED_BIT, Ordering::AcqRel);
        let count = state & CLAIMS_COUNT_MASK;

        assert_eq!(
            state & CLAIMS_CLOSED_BIT,
            0,
            "storage closed more than once"
        );
        assert_ne!(count, 0, "storage closed without an owning claim");

        CloseSnapshot { claim_count: count }
    }

    pub(super) fn release(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);

        loop {
            let count = state & CLAIMS_COUNT_MASK;
            assert_ne!(count, 0, "claim released more often than taken");
            assert!(
                count != 1 || state & CLAIMS_CLOSED_BIT != 0,
                "last claim released before storage was closed"
            );

            match self.state.compare_exchange_weak(
                state,
                state - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return count == 1,
                Err(actual) => state = actual,
            }
        }
    }
}

/// A storage for an instance binding.
///
/// # Safety
///
/// [`is_bound()`](Storage::is_bound()) must return `true` if any references to the stored user instance
/// exists.
///
/// It must be safe to drop this storage if we have a `&mut` reference to the storage and  
/// [`is_bound()`](Storage::is_bound()) returns `false`.
pub unsafe trait Storage {
    /// The type of instances stored by this storage.
    type Instance: GodotClass;

    /// Constructs a new storage for an instance binding referencing `user_instance`.
    fn construct(
        user_instance: Self::Instance,
        base: Base<<Self::Instance as GodotClass>::Base>,
    ) -> Self;

    /// Returns `true` when there are any outstanding references to this storage's instance.
    fn is_bound(&self) -> bool;

    /// The base object that this storage contains.
    fn base(&self) -> &Base<<Self::Instance as GodotClass>::Base>;

    /// Returns a shared reference to this storage's instance.
    ///
    /// This will ensure Rust's rules surrounding references are upheld. Possibly panicking at runtime if
    /// they are violated.
    fn get(&self) -> RefGuard<'_, Self::Instance>;

    /// Returns a mutable/exclusive reference to this storage's instance.
    ///
    /// This will ensure Rust's rules surrounding references are upheld. Possibly panicking at runtime if
    /// they are violated.
    fn get_mut(&self) -> MutGuard<'_, Self::Instance>;

    /// Returns a guard that allows calling methods on `Gd<Base>` that take `&mut self`.
    ///
    /// This can use the provided `instance` to provide extra safety guarantees such as allowing reentrant
    /// code to create new mutable references.
    fn get_inaccessible<'a: 'b, 'b>(
        &'a self,
        instance: &'b mut Self::Instance,
    ) -> InaccessibleGuard<'b, Self::Instance>;

    /// Returns whether this storage is currently alive or being destroyed.
    ///
    /// This is purely informational and cannot be relied on for safety.
    fn get_lifecycle(&self) -> Lifecycle;

    /// Mark this storage as currently alive or being destroyed.
    ///
    /// This is purely informational and thus is safe to set to whatever value, but it should still be set as
    /// expected.
    fn set_lifecycle(&self, lifecycle: Lifecycle);

    /// Get a `Gd` referencing the Godot object.
    fn get_gd(&self) -> Gd<Self::Instance>
    where
        Self::Instance: Inherits<<Self::Instance as GodotClass>::Base>,
    {
        self.base().__constructed_gd().cast()
    }

    /// Puts self onto the heap and returns a pointer to this new heap-allocation.
    ///
    /// The allocation is freed once the last claim on it is released; see `StorageClaim`.
    #[must_use]
    fn into_raw(self) -> *mut Self
    where
        Self: Sized,
    {
        Box::into_raw(Box::new(self))
    }

    fn mark_destroyed_by_godot(&self) {
        self.set_lifecycle(Lifecycle::Destroying);

        log_pre_drop(self);
    }

    /*#[inline(always)]
    fn destroyed_by_godot(&self) -> bool {
        out!(
            "    is_d;  self={:?}, val={:?}, obj={:?}",
            self as *const _,
            self.get_lifecycle(),
            self.base(),
        );
        matches!(self.get_lifecycle(), Lifecycle::Destroying)
    }*/
}

/// An internal trait for keeping track of reference counts for a storage.
pub(crate) trait StorageRefCounted: Storage {
    fn on_inc_ref(&self);

    fn on_dec_ref(&self);
}

pub enum InstanceStorage<T: GodotClass> {
    Single(crate::storage::single_threaded::InstanceStorage<T>),
    Blocking(crate::storage::multi_threaded::InstanceStorage<T>),
}

unsafe impl<T: GodotClass> Storage for InstanceStorage<T> {
    type Instance = T;

    fn construct(user_instance: T, base: Base<T::Base>) -> Self {
        match T::__THREADING {
            ClassThreading::SingleThreaded => Self::Single(
                crate::storage::single_threaded::InstanceStorage::construct(user_instance, base),
            ),
            ClassThreading::Blocking => Self::Blocking(
                crate::storage::multi_threaded::InstanceStorage::construct(user_instance, base),
            ),
        }
    }

    fn is_bound(&self) -> bool {
        match self {
            Self::Single(storage) => storage.is_bound(),
            Self::Blocking(storage) => storage.is_bound(),
        }
    }

    fn base(&self) -> &Base<T::Base> {
        match self {
            Self::Single(storage) => storage.base(),
            Self::Blocking(storage) => storage.base(),
        }
    }

    fn get(&self) -> RefGuard<'_, T> {
        match self {
            Self::Single(storage) => storage.get(),
            Self::Blocking(storage) => storage.get(),
        }
    }

    fn get_mut(&self) -> MutGuard<'_, T> {
        match self {
            Self::Single(storage) => storage.get_mut(),
            Self::Blocking(storage) => storage.get_mut(),
        }
    }

    fn get_inaccessible<'a: 'b, 'b>(&'a self, instance: &'b mut T) -> InaccessibleGuard<'b, T> {
        match self {
            Self::Single(storage) => storage.get_inaccessible(instance),
            Self::Blocking(storage) => storage.get_inaccessible(instance),
        }
    }

    fn get_lifecycle(&self) -> Lifecycle {
        match self {
            Self::Single(storage) => storage.get_lifecycle(),
            Self::Blocking(storage) => storage.get_lifecycle(),
        }
    }

    fn set_lifecycle(&self, lifecycle: Lifecycle) {
        match self {
            Self::Single(storage) => storage.set_lifecycle(lifecycle),
            Self::Blocking(storage) => storage.set_lifecycle(lifecycle),
        }
    }
}

impl<T: GodotClass> InstanceStorage<T> {
    /// Puts this storage on the heap and returns the opaque instance token Godot must retain.
    ///
    /// Single-threaded classes preserve the direct-pointer representation. Blocking classes use a never-reused registry handle, so a
    /// callback racing destruction can first acquire its lifetime claim without dereferencing an address that may already have been freed.
    pub(crate) fn into_instance_ptr(self) -> sys::GDExtensionClassInstancePtr {
        let mut storage = Box::new(self);
        let storage_ptr: *mut Self = &mut *storage;

        match T::__THREADING {
            ClassThreading::SingleThreaded => Box::into_raw(storage).cast(),
            ClassThreading::Blocking => {
                let handle = instance_registry::register(storage_ptr).unwrap_or_else(|error| {
                    panic!("failed to register blocking instance storage: {error}")
                });

                // Registration now owns the responsibility for making this allocation discoverable until its last claim. No callback can
                // observe the handle before this method returns it to the engine, and every destruction path removes the entry first.
                std::mem::forget(storage);
                handle.into_instance_ptr()
            }
        }
    }

    pub(super) fn try_inc_claims(&self) -> bool {
        match self {
            Self::Single(storage) => storage.try_inc_claims(),
            Self::Blocking(storage) => storage.try_inc_claims(),
        }
    }

    pub(super) fn begin_close_claims(&self) -> CloseSnapshot {
        match self {
            Self::Single(storage) => storage.begin_close_claims(),
            Self::Blocking(storage) => storage.begin_close_claims(),
        }
    }

    pub(super) fn dec_claims(&self) -> bool {
        match self {
            Self::Single(storage) => storage.dec_claims(),
            Self::Blocking(storage) => storage.dec_claims(),
        }
    }
}

impl<T: GodotClass> StorageRefCounted for InstanceStorage<T> {
    fn on_inc_ref(&self) {
        match self {
            Self::Single(storage) => storage.on_inc_ref(),
            Self::Blocking(storage) => storage.on_inc_ref(),
        }
    }

    fn on_dec_ref(&self) {
        match self {
            Self::Single(storage) => storage.on_dec_ref(),
            Self::Blocking(storage) => storage.on_dec_ref(),
        }
    }
}

const fn _assert_implements_storage<T: Storage + StorageRefCounted>() {}

const _INSTANCE_STORAGE_IMPLEMENTS_STORAGE: () =
    _assert_implements_storage::<InstanceStorage<crate::classes::Object>>();

/// Wrapper to handle multiple receivers type, without exposing the Storage itself.
#[doc(hidden)]
pub struct VirtualMethodReceiver<'a, T: GodotClass> {
    inner: VirtualMethodReceiverInner<'a, T>,
}

enum VirtualMethodReceiverInner<'a, T: GodotClass> {
    /// &self.
    Ref(RefGuard<'a, T>),
    /// &mut self.
    Mut(MutGuard<'a, T>),
    /// this: Gd<Self>.
    GdSelf(Gd<T>),
    /// Implementation detail – required to swap the values.
    Uninit,
}

impl<'a, T: GodotClass> VirtualMethodReceiver<'a, T> {
    pub fn recv_gd(mut self) -> Gd<T> {
        match std::mem::replace(&mut self.inner, VirtualMethodReceiverInner::Uninit) {
            VirtualMethodReceiverInner::GdSelf(instance) => instance,
            _ => panic!("Tried to use Gd<Self> receiver for method which doesn't accept it."),
        }
    }

    pub fn recv_self(mut self) -> impl Deref<Target = T> + use<'a, T> {
        match std::mem::replace(&mut self.inner, VirtualMethodReceiverInner::Uninit) {
            VirtualMethodReceiverInner::Ref(instance) => instance,
            _ => panic!("Tried to use &self receiver for method which doesn't accept it."),
        }
    }

    pub fn recv_self_mut(mut self) -> impl DerefMut<Target = T> + use<'a, T> {
        match std::mem::replace(&mut self.inner, VirtualMethodReceiverInner::Uninit) {
            VirtualMethodReceiverInner::Mut(instance) => instance,
            _ => panic!("Tried to use &mut self receiver for method which doesn't accept it."),
        }
    }
}

// Marker structs.
// Used to extract proper type from storage and pass it to public API while defined as an associated item on the trait (`T::Recv::instance(storage)`).

#[doc(hidden)]
pub enum RecvRef {}
#[doc(hidden)]
pub enum RecvMut {}
#[doc(hidden)]
pub enum RecvGdSelf {}

#[doc(hidden)]
pub trait IntoVirtualMethodReceiver<T: GodotClass> {
    #[doc(hidden)]
    fn instance<'a, 'b: 'a>(storage: &'b InstanceStorage<T>) -> VirtualMethodReceiver<'a, T>;
}

impl<T: GodotClass> IntoVirtualMethodReceiver<T> for RecvRef {
    fn instance<'a, 'b: 'a>(storage: &'b InstanceStorage<T>) -> VirtualMethodReceiver<'a, T> {
        VirtualMethodReceiver {
            inner: VirtualMethodReceiverInner::Ref(storage.get()),
        }
    }
}

impl<T: GodotClass> IntoVirtualMethodReceiver<T> for RecvMut {
    fn instance<'a, 'b: 'a>(storage: &'b InstanceStorage<T>) -> VirtualMethodReceiver<'a, T> {
        VirtualMethodReceiver {
            inner: VirtualMethodReceiverInner::Mut(storage.get_mut()),
        }
    }
}

impl<T> IntoVirtualMethodReceiver<T> for RecvGdSelf
where
    T: Inherits<<T as GodotClass>::Base>,
{
    fn instance<'a, 'b: 'a>(storage: &'b InstanceStorage<T>) -> VirtualMethodReceiver<'a, T> {
        VirtualMethodReceiver {
            inner: VirtualMethodReceiverInner::GdSelf(storage.get_gd()),
        }
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Storage claims. The free functions sit at the FFI boundary: they turn Godot's opaque instance pointer into typed access.

/// Keeps the Rust instance storage alive during a Godot -> Rust call.
///
/// User code can drop the last reference to the receiver mid-call (e.g. a signal handler nulling the emitter), or `free()` it. The Godot object
/// then dies while a `GdRef`/`GdMut` guard is still active.
///
/// The storage counts claims: Godot holds one from construction to destruction, each ongoing call holds one. Removing the last claim frees the
/// storage. Guards borrow the claim, so they drop first. A counter rather than a flag, to handle nested calls; atomic for blocking storage.
///
/// Only the storage survives, not the Godot object -- godot-rust cannot prevent its destruction: `Ref::unref()` decides to `memdelete` before
/// the extension's `unreference` callback runs, and `free()` is immediate. Holding a strong reference for the call's duration, like GDScript
/// does, would not help either: it drops before the trampoline returns, fails at refcount 0, and costs ~40% per call. So for the rest of the
/// call, field access works but base operations panic and `Drop` sees a dead base. To survive the call, an object needs a reference that
/// outlives it: `self.to_gd().run_deferred_gd(|_| {})`.
///
/// Every callback which can overlap destruction, including `reference` and `unreference`, takes a claim. The `free` callback closes admission
/// while its distinguished Godot claim still pins the allocation.
#[must_use = "dropping the claim immediately releases it, which may free the storage"]
pub struct StorageClaim<T: GodotClass> {
    // Raw pointer rather than reference: the claim owns no borrow (other claims may exist), and freeing needs write access to the whole
    // allocation for drop(&mut). Using a &InstanceStorage shared-ref would be unsound as it would eventually be promoted to &mut.
    storage: *mut InstanceStorage<T>,
    // Present only for blocking storage. It identifies the registry entry which must be removed before the allocation can be freed.
    handle: Option<InstanceHandle>,
}

impl<T: GodotClass> std::fmt::Debug for StorageClaim<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageClaim")
            .field("storage", &self.storage)
            .field("handle", &self.handle)
            .finish()
    }
}

impl<T: GodotClass> StorageClaim<T> {
    /// Returns the claimed storage with a caller-chosen lifetime.
    ///
    /// # Safety
    /// The returned reference and every value borrowing it must be dropped before this claim. The allocation itself is pinned.
    pub(crate) unsafe fn storage_unbounded<'a>(&self) -> &'a InstanceStorage<T> {
        // SAFETY: a live claim prevents deallocation, and the caller is responsible for keeping this claim alive for `'a`.
        unsafe { &*self.storage }
    }
}

impl<T: GodotClass> Deref for StorageClaim<T> {
    type Target = InstanceStorage<T>;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the claim keeps the storage alive.
        unsafe { &*self.storage }
    }
}

impl<T: GodotClass> Drop for StorageClaim<T> {
    fn drop(&mut self) {
        if self.dec_claims() {
            // SAFETY: this was the last claim, and the guards borrowing it are already dropped.
            unsafe { deallocate_storage(self.storage, self.handle) };
        }
    }
}

/// Deallocates the storage, once no claim is left.
///
/// # Safety
/// The last claim on `storage` must have been released, and no reference into it may be live.
unsafe fn deallocate_storage<T: GodotClass>(
    storage: *mut InstanceStorage<T>,
    handle: Option<InstanceHandle>,
) {
    if let Some(handle) = handle {
        // Removal must precede every later read and `Box::from_raw`: once the last claim is gone, no new lookup may recover this allocation.
        // If the defensive `is_bound()` check below detects broken bookkeeping, we intentionally leak the allocation so the stray Rust
        // reference remains valid, but the handle still stays retired; retaining the registry entry would expose a destroyed zombie to new
        // weak lookups.
        assert!(
            instance_registry::remove(handle, storage),
            "blocking instance storage registry entry missing or mismatched during deallocation"
        );
    }

    // SAFETY: delegated to caller.
    let storage_ref = unsafe { &*storage };

    // Every GdRef/GdMut owns a claim, and callback guards borrow a callback claim. At count zero admission is closed, so no new guard can
    // start. A bound value here therefore indicates an internal lifetime-invariant violation; retain the allocation rather than invalidating
    // the reference.
    if storage_ref.is_bound() {
        report_destruction_while_bound(storage_ref); // May crash process, depending on safeguard level.
        return;
    }

    // SAFETY: no claim is left and admission is closed, so no callback, bind guard, or in-progress blocking borrow can exist or start.
    // `is_bound()` ruled out an internal bookkeeping violation. Dropping is therefore allowed by `Storage`'s safety contract. The allocation
    // stems from `InstanceStorage::into_instance_ptr()`.
    let _drop = unsafe { Box::from_raw(storage) };
}

/// Interprets a single-threaded class's opaque pointer as pointing directly to `InstanceStorage<T>`.
fn direct_storage_ptr<T: GodotClass>(
    instance_ptr: sys::GDExtensionClassInstancePtr,
) -> *mut InstanceStorage<T> {
    instance_ptr as *mut InstanceStorage<T>
}

/// Interprets the opaque pointer as pointing to `InstanceStorage<T>`, keeping the storage alive until the returned claim drops.
///
/// # Safety
/// `instance_ptr` is assumed to point to a valid instance.
///
/// The claim keeps the storage (Rust object) alive, not the Godot object -- the latter may die mid-call.
pub unsafe fn as_storage<T: GodotClass>(
    instance_ptr: sys::GDExtensionClassInstancePtr,
) -> StorageClaim<T> {
    // SAFETY: delegated to caller.
    unsafe { try_as_storage(instance_ptr) }.unwrap_or_else(|| {
        panic!(
            "Godot callback tried to access {} after instance destruction began",
            std::any::type_name::<T>()
        )
    })
}

/// Tries to claim a live instance storage. Returns `None` once its free callback has begun closing claim admission.
///
/// # Safety
/// For single-threaded storage, `instance_ptr` must point to the still-allocated claim header. For blocking storage it must be an opaque handle
/// previously issued for `T`; stale handles are safely rejected. A successful claim keeps the whole storage alive until the returned value
/// drops.
pub unsafe fn try_as_storage<T: GodotClass>(
    instance_ptr: sys::GDExtensionClassInstancePtr,
) -> Option<StorageClaim<T>> {
    match T::__THREADING {
        ClassThreading::SingleThreaded => {
            let storage = direct_storage_ptr::<T>(instance_ptr);

            // SAFETY: delegated to caller. Single-threaded callbacks cannot race deallocation, and the local claim state still rejects
            // logically late access after close.
            if !unsafe { (*storage).try_inc_claims() } {
                return None;
            }

            Some(StorageClaim {
                storage,
                handle: None,
            })
        }
        ClassThreading::Blocking => {
            let handle = InstanceHandle::from_instance_ptr(instance_ptr)?;
            let storage =
                instance_registry::with_read::<InstanceStorage<T>, _>(handle, |storage| {
                    // SAFETY: the registry read lock prevents close/removal while claim admission examines this allocation.
                    unsafe { (*storage).try_inc_claims() }.then_some(storage)
                })??;

            Some(StorageClaim {
                storage,
                handle: Some(handle),
            })
        }
    }
}

/// Like [`as_storage()`], but *weak*: the returned reference does not keep the storage alive.
///
/// This is only for code whose caller independently proves that destruction cannot overlap (for example a live, thread-confined `Gd`). FFI
/// callbacks which may race the free callback must use [`try_as_storage()`] instead.
///
/// # Safety
/// Same as [`as_storage()`]. Additionally, the storage must not be destroyed while the returned reference is live.
pub unsafe fn as_weak_storage<'u, T: GodotClass>(
    instance_ptr: sys::GDExtensionClassInstancePtr,
) -> &'u InstanceStorage<T> {
    let storage = match T::__THREADING {
        ClassThreading::SingleThreaded => direct_storage_ptr::<T>(instance_ptr),
        ClassThreading::Blocking => {
            let handle = InstanceHandle::from_instance_ptr(instance_ptr)
                .expect("null blocking instance storage handle");
            instance_registry::with_read::<InstanceStorage<T>, _>(handle, |storage| storage)
                .unwrap_or_else(|| {
                    panic!(
                        "Godot callback tried to weakly access {} after its storage was unregistered",
                        std::any::type_name::<T>()
                    )
                })
        }
    };

    // SAFETY: delegated to caller. For blocking storage the registry lookup preserves pointer provenance; the caller's independent lifetime
    // proof (normally a live Godot or callback claim) keeps the entry and allocation alive after the read lock is released.
    unsafe { &*storage }
}

/// Releases the claim Godot took when the storage was constructed; see [`StorageClaim`].
///
/// Deallocates the storage, unless a Godot -> Rust call still holds a claim -- then the last of those calls deallocates it.
///
/// # Safety
/// `instance_ptr` is assumed to identify a valid instance. This function must only be invoked once for a pointer or handle.
pub unsafe fn release_godot_claim<T: GodotClass>(instance_ptr: sys::GDExtensionClassInstancePtr) {
    let (storage, handle, close_snapshot) = match T::__THREADING {
        ClassThreading::SingleThreaded => {
            let storage = direct_storage_ptr::<T>(instance_ptr);
            // SAFETY: Godot's distinguished claim pins direct single-threaded storage for this callback.
            let snapshot = unsafe { (*storage).begin_close_claims() };
            (storage, None, snapshot)
        }
        ClassThreading::Blocking => {
            let handle = InstanceHandle::from_instance_ptr(instance_ptr)
                .expect("null blocking instance storage handle in free callback");
            let (storage, snapshot) = instance_registry::with_write::<InstanceStorage<T>, _>(
                handle,
                |storage| {
                    // SAFETY: the registry write lock excludes every pre-claim lookup. Godot's distinguished claim keeps the allocation live.
                    let snapshot = unsafe { (*storage).begin_close_claims() };
                    (storage, snapshot)
                },
            )
            .unwrap_or_else(|| {
                panic!(
                    "blocking instance storage for {} was missing or mismatched in free callback",
                    std::any::type_name::<T>()
                )
            });
            (storage, Some(handle), snapshot)
        }
    };

    release_godot_claim_ordered(
        // Close was already linearized under the registry write lock for blocking storage, or directly on this thread for single storage.
        || close_snapshot,
        |snapshot| {
            // SAFETY: this callback runs before releasing Godot's claim.
            let storage_ref = unsafe { &*storage };
            storage_ref.mark_destroyed_by_godot();

            if snapshot.has_call_claims() {
                // Object died while a call still holds a claim, no matter if the last reference was dropped or `free()` was called. Both
                // leave the engine with a dangling `this` for the rest of the call.
                report_destruction_during_call(storage_ref);
            }
        },
        // SAFETY: Godot's claim exists since construction, and the caller guarantees to release it only once.
        || unsafe { (*storage).dec_claims() },
        // SAFETY: invoked only when releasing Godot's claim left no claim alive.
        || unsafe { deallocate_storage(storage, handle) },
    );
}

/// Reports while the caller-owned Godot claim still pins the allocation, then releases that claim.
///
/// Keeping this ordering in one testable function prevents a last concurrent call claim from freeing storage between claim release and the
/// diagnostic's storage access.
fn release_godot_claim_ordered(
    begin_close: impl FnOnce() -> CloseSnapshot,
    while_pinned: impl FnOnce(CloseSnapshot),
    release_claim: impl FnOnce() -> bool,
    deallocate: impl FnOnce(),
) {
    let snapshot = begin_close();
    while_pinned(snapshot);

    if release_claim() {
        deallocate();
    }
}

/// Reports destruction during Godot->Rust call; see <https://github.com/godot-rust/gdext/pull/1671>.
fn report_destruction_during_call<T: GodotClass>(storage: &InstanceStorage<T>) {
    let class_id = T::class_id();
    let instance_id = storage.base().instance_id_unchecked();

    sys::defer_startup_error!(
        once_per: class_id;
        "Godot object destroyed during Godot->Rust call; Gd {{ id: {instance_id}, class: {class_id} }}.\n  \
        Engine may access freed object afterwards, which is UB. Reported once per class.\n  \
        Quick'n'dirty workarounds (prefer clean destruction order):\n  \
        * Ref-counted objects: keep alive with self.to_gd().run_deferred_gd(|_| {{}})\n  \
        * Manually managed objects: defer the free() to idle time, e.g. using queue_free()."
    );
}

/// Reports that the storage is destroyed while the Rust object is still borrowed. May crash the process, depending on safeguard level.
fn report_destruction_while_bound<T: GodotClass>(storage: &InstanceStorage<T>) {
    // We cannot panic here, since this code is invoked from a C callback. Panicking would mean unwinding into C code, which is UB.
    // We have the following options:
    // 1. Print an error as a best-effort, knowing that UB is likely to occur whenever the user will access &T or &mut T. (Technically, the
    //    mere existence of these references is UB since the T is dead.)
    // 2. Abort the process. This is the safest option, but a very drastic measure, and not what gdext does elsewhere.
    //    We can use Godot's OS.crash() API here.
    // 3. Change everything to "C-unwind" API. Would make the FFI unwinding safe, but still not clear if Godot would handle it appropriately.
    //    Even if yes, it's likely the same behavior as OS.crash().
    // 4. Prevent destruction of the Rust part (InstanceStorage). This would solve the immediate problem of &T and &mut T becoming invalid,
    //    but it would leave a zombie object behind, where all base operations and Godot interactions suddenly fail, which likely creates
    //    its own set of edge cases. It would _also_ make the problem less observable, since the user can keep interacting with the Rust
    //    object and slowly accumulate memory leaks.
    //    - Letting Gd<T> and InstanceStorage<T> know about this specific object state and panicking in the next Rust call might be an option,
    //      but we still can't control direct access to the T.
    //
    // For now we choose option 2 in strict+balanced levels, and 4 in disengaged level.
    let error = format!(
        "Godot object destroyed during active bind() or bind_mut() guard; {:?}.\n  \
        This is a bug in your code that may cause UB and logic errors. Make sure that objects are not\n  \
        destroyed while you still hold a Rust reference to them, or use Gd::free() which is safe.",
        storage.base()
    );

    // In strict+balanced level, crash which may trigger breakpoint.
    // In disengaged level, leak Rust object (Godot philosophy: don't crash if somehow avoidable). Likely leads to follow-up issues.
    if cfg!(safeguards_balanced) {
        let error = crate::builtin::GString::from(&error);
        crate::classes::Os::singleton().crash(&error);
    } else {
        godot_error!("{}", error);
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// InstanceCache polymorphism, no-op for engine-defined types

pub(crate) trait InstanceCache: Clone {
    fn null() -> Self;
}

impl InstanceCache for () {
    fn null() -> Self {} // returns ()
}

impl InstanceCache for Cell<sys::GDExtensionClassInstancePtr> {
    fn null() -> Self {
        Cell::new(ptr::null_mut())
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Callbacks

pub fn nop_instance_callbacks() -> sys::GDExtensionInstanceBindingCallbacks {
    // These could also be null pointers, if they are definitely not invoked (e.g. create_callback only passed to object_get_instance_binding(),
    // when there is already a binding). Current "empty but not null" impl corresponds to godot-cpp (wrapped.hpp).
    sys::GDExtensionInstanceBindingCallbacks {
        create_callback: Some(create_callback),
        free_callback: Some(free_callback),
        reference_callback: Some(reference_callback),
    }
}

extern "C" fn create_callback(
    _p_token: *mut std::os::raw::c_void,
    _p_instance: *mut std::os::raw::c_void,
) -> *mut std::os::raw::c_void {
    // There is no "instance binding" for Godot types like Node3D -- this would be the user-defined Rust class
    ptr::null_mut()
}

extern "C" fn free_callback(
    _p_token: *mut std::os::raw::c_void,
    _p_instance: *mut std::os::raw::c_void,
    _p_binding: *mut std::os::raw::c_void,
) {
}

extern "C" fn reference_callback(
    _p_token: *mut std::os::raw::c_void,
    _p_binding: *mut std::os::raw::c_void,
    _p_reference: sys::GDExtensionBool,
) -> sys::GDExtensionBool {
    true as u8
}

#[cfg(test)]
mod claim_tests {
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::thread;

    use super::{
        AtomicClaimState, AtomicClaimWord, CLAIMS_COUNT_MASK, InstanceStorage, LocalClaimState,
        direct_storage_ptr, release_godot_claim_ordered,
    };
    use crate::classes::Object;

    #[cfg(not(target_family = "wasm"))]
    impl AtomicClaimWord for loom::sync::atomic::AtomicU32 {
        fn load(&self, order: Ordering) -> u32 {
            self.load(order)
        }

        fn fetch_or(&self, value: u32, order: Ordering) -> u32 {
            self.fetch_or(value, order)
        }

        fn compare_exchange_weak(
            &self,
            current: u32,
            new: u32,
            success: Ordering,
            failure: Ordering,
        ) -> Result<u32, u32> {
            self.compare_exchange_weak(current, new, success, failure)
        }
    }

    #[test]
    fn godot_claim_pins_storage_through_concurrent_destruction_report() {
        let claims = AtomicClaimState::new();
        assert!(claims.try_claim(), "simulate an admitted callback claim");

        let alive = AtomicBool::new(true);
        let deallocations = AtomicU32::new(0);
        let (report_started_tx, report_started_rx) = mpsc::sync_channel(0);
        let (callback_released_tx, callback_released_rx) = mpsc::sync_channel(0);

        thread::scope(|scope| {
            let claims_ref = &claims;
            let alive_ref = &alive;
            let deallocations_ref = &deallocations;
            let callback = scope.spawn(move || {
                report_started_rx.recv().unwrap();

                if claims_ref.release() {
                    alive_ref.store(false, Ordering::SeqCst);
                    deallocations_ref.fetch_add(1, Ordering::SeqCst);
                }

                callback_released_tx.send(()).unwrap();
            });

            release_godot_claim_ordered(
                || claims.begin_close(),
                |snapshot| {
                    assert!(snapshot.has_call_claims());
                    report_started_tx.send(()).unwrap();
                    callback_released_rx.recv().unwrap();

                    // The callback claim is gone, but the diagnostic can still read storage because Godot's claim has not been released.
                    assert!(alive.load(Ordering::SeqCst));
                },
                || claims.release(),
                || {
                    assert!(alive.swap(false, Ordering::SeqCst));
                    deallocations.fetch_add(1, Ordering::SeqCst);
                },
            );

            callback.join().unwrap();
        });

        assert!(!alive.load(Ordering::SeqCst));
        assert_eq!(deallocations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn close_rejects_late_claim_and_zero_cannot_be_resurrected() {
        let claims = AtomicClaimState::new();

        let snapshot = claims.begin_close();
        assert!(!snapshot.has_call_claims());
        assert!(!claims.try_claim());
        assert!(claims.release());
        assert!(!claims.try_claim());
    }

    #[cfg(not(target_family = "wasm"))]
    fn check_claim_close_model(worker_count: usize, preemption_bound: Option<usize>) {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = preemption_bound;
        model.check(move || {
            use loom::sync::Arc;
            use loom::sync::atomic::{AtomicU32 as LoomAtomicU32, AtomicUsize};
            use loom::thread;

            let claims = Arc::new(AtomicClaimState::<LoomAtomicU32> {
                state: LoomAtomicU32::new(1),
            });
            let deallocations = Arc::new(AtomicUsize::new(0));

            let workers: Vec<_> = (0..worker_count)
                .map(|_| {
                    let worker_claims = Arc::clone(&claims);
                    let worker_deallocations = Arc::clone(&deallocations);
                    thread::spawn(move || {
                        if worker_claims.try_claim() && worker_claims.release() {
                            worker_deallocations.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                })
                .collect();

            let closer_claims = Arc::clone(&claims);
            let closer_deallocations = Arc::clone(&deallocations);
            let closer = thread::spawn(move || {
                closer_claims.begin_close();
                if closer_claims.release() {
                    closer_deallocations.fetch_add(1, Ordering::SeqCst);
                }
            });

            for worker in workers {
                worker.join().unwrap();
            }
            closer.join().unwrap();

            assert_eq!(deallocations.load(Ordering::SeqCst), 1);
            assert!(!claims.try_claim());
        });
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn loom_exhausts_one_claim_against_close_and_exactly_one_side_deallocates() {
        check_claim_close_model(1, None);
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn loom_bounds_two_claims_against_close_and_exactly_one_side_deallocates() {
        // Two simultaneous callbacks plus teardown produce an unbounded number of equivalent retries because production deliberately uses
        // compare_exchange_weak. The bound keeps that state space tractable while still exercising multi-claim close/release schedules.
        check_claim_close_model(2, Some(2));
    }

    #[test]
    fn local_claims_follow_the_same_close_protocol() {
        let claims = LocalClaimState::new();
        assert!(claims.try_claim());

        let snapshot = claims.begin_close();
        assert!(snapshot.has_call_claims());
        assert!(!claims.try_claim());
        assert!(!claims.release());
        assert!(claims.release());
        assert!(!claims.try_claim());
    }

    #[test]
    fn claim_counter_rejects_overflow() {
        let claims = AtomicClaimState {
            state: AtomicU32::new(CLAIMS_COUNT_MASK),
        };

        assert!(!claims.try_claim());
    }

    #[test]
    fn single_threaded_direct_pointer_representation_is_unchanged() {
        let storage = NonNull::<InstanceStorage<Object>>::dangling().as_ptr();
        let instance_ptr = storage.cast();

        assert_eq!(direct_storage_ptr::<Object>(instance_ptr), storage);
    }
}
