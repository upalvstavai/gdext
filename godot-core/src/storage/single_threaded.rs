/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::cell;

use godot_cell::panicking::GdCell;

use crate::obj::{Base, GodotClass};
use crate::storage::{
    CloseSnapshot, DebugBorrowTracker, InaccessibleGuard, Lifecycle, LocalClaimState, MutGuard,
    RefGuard, Storage, StorageRefCounted,
};

pub struct InstanceStorage<T: GodotClass> {
    user_instance: GdCell<T>,
    pub(super) base: Base<T::Base>,

    // Declared after `user_instance`, is dropped last.
    pub(super) lifecycle: cell::Cell<Lifecycle>,

    // Claims on this storage's lifetime: 1 from Godot, one from each ongoing Godot->Rust call. See `StorageClaim`.
    claims: LocalClaimState,

    // No-op in Release mode.
    borrow_tracker: DebugBorrowTracker,
}

// SAFETY:
// The only way to get a reference to the user instance is by going through the `GdCell` in `user_instance`.
// If this `GdCell` has returned any references, then `self.user_instance.as_ref().is_currently_bound()` will
// return true. So `is_bound` will return true when a reference to the user instance exists.
//
// If `is_bound` is false, then there are no references to the user instance in this storage. And if a `&mut`
// reference to the storage exists then no other references to data in this storage can exist. So we can
// safely drop it.
unsafe impl<T: GodotClass> Storage for InstanceStorage<T> {
    type Instance = T;

    fn construct(
        user_instance: Self::Instance,
        base: Base<<Self::Instance as GodotClass>::Base>,
    ) -> Self {
        super::log_construct::<T>(&base);

        Self {
            user_instance: GdCell::new(user_instance),
            base,
            lifecycle: cell::Cell::new(Lifecycle::Alive),
            claims: LocalClaimState::new(),
            borrow_tracker: DebugBorrowTracker::new(),
        }
    }

    fn is_bound(&self) -> bool {
        self.user_instance.is_currently_bound()
    }

    fn base(&self) -> &Base<<Self::Instance as GodotClass>::Base> {
        &self.base
    }

    fn get(&self) -> RefGuard<'_, T> {
        let guard = self
            .user_instance
            .borrow()
            .unwrap_or_else(|e| super::bind_failed::<T>(e, &self.borrow_tracker));

        self.borrow_tracker.track_ref_borrow();
        RefGuard::Single(guard)
    }

    fn get_mut(&self) -> MutGuard<'_, T> {
        let guard = self
            .user_instance
            .borrow_mut()
            .unwrap_or_else(|e| super::bind_mut_failed::<T>(e, &self.borrow_tracker));

        self.borrow_tracker.track_mut_borrow();
        MutGuard::Single(guard)
    }

    fn get_inaccessible<'stor: 'inst, 'inst>(
        &'stor self,
        value: &'inst mut Self::Instance,
    ) -> InaccessibleGuard<'inst, T> {
        self.user_instance
            .make_inaccessible(value)
            .map(InaccessibleGuard::Single)
            .unwrap_or_else(|e| super::bug_inaccessible::<T>(e))
    }

    fn get_lifecycle(&self) -> Lifecycle {
        self.lifecycle.get()
    }

    fn set_lifecycle(&self, lifecycle: Lifecycle) {
        self.lifecycle.set(lifecycle)
    }
}

impl<T: GodotClass> InstanceStorage<T> {
    /// Tries to add a claim; see [`StorageClaim`][crate::storage::StorageClaim].
    pub(super) fn try_inc_claims(&self) -> bool {
        self.claims.try_claim()
    }

    pub(super) fn begin_close_claims(&self) -> CloseSnapshot {
        self.claims.begin_close()
    }

    /// Removes a claim; returns `true` if it was the last one.
    pub(super) fn dec_claims(&self) -> bool {
        self.claims.release()
    }
}

impl<T: GodotClass> StorageRefCounted for InstanceStorage<T> {
    fn on_inc_ref(&self) {
        // Note: on_inc_ref() and on_dec_ref() do not track extra strong references from Base::to_init_gd().
        // See https://github.com/godot-rust/gdext/pull/1273 for code that had it.

        super::log_inc_ref(self);
    }

    fn on_dec_ref(&self) {
        // IMPORTANT: it is too late here to perform dec-ref operations on the Base (for "surplus" strong references).
        // This callback is only invoked in the C++ condition `if (rc_val <= 1 /* higher is not relevant */)` -- see Godot ref_counted.cpp.
        // The T <-> RefCounted hierarchical relation is usually already broken up at this point, and further dec-ref may bring the count
        // down to 0.

        super::log_dec_ref(self);
    }
}

impl<T: GodotClass> Drop for InstanceStorage<T> {
    fn drop(&mut self) {
        super::log_drop(self);
    }
}
