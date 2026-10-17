/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use crate::{
    BuiltinLifecycleTable, BuiltinMethodTable, ClassCoreMethodTable, ClassEditorMethodTable,
    ClassSceneMethodTable, ClassServersMethodTable, GDExtensionClassLibraryPtr,
    GDExtensionConstTypePtr, GDExtensionInterface, GDExtensionTypePtr,
    GDExtensionUninitializedTypePtr, GDExtensionUninitializedVariantPtr, GDExtensionVariantPtr,
    GdextRuntimeMetadata, ManualInitCell, UtilityFunctionTable,
};

#[cfg(loom)]
use loom::sync::{
    Condvar as CallbackCondvar, Mutex as CallbackMutex, MutexGuard as CallbackMutexGuard,
};
#[cfg(loom)]
use loom::thread::ThreadId as CallbackThreadId;
#[cfg(not(loom))]
use std::sync::{
    Condvar as CallbackCondvar, Mutex as CallbackMutex, MutexGuard as CallbackMutexGuard,
};
#[cfg(all(not(loom), not(wasm_nothreads)))]
use std::thread::ThreadId as CallbackThreadId;
#[cfg(all(not(loom), wasm_nothreads))]
type CallbackThreadId = ();

#[cfg(loom)]
fn current_callback_thread_id() -> CallbackThreadId {
    loom::thread::current().id()
}

#[cfg(all(not(loom), not(wasm_nothreads)))]
fn current_callback_thread_id() -> CallbackThreadId {
    crate::current_thread_id()
}

#[cfg(all(not(loom), wasm_nothreads))]
fn current_callback_thread_id() -> CallbackThreadId {}

thread_local! {
    static THREADED_CALLBACK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(not(loom))]
thread_local! {
    static THREADED_CALLBACK_ADMISSION_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(loom)]
loom::thread_local! {
    static THREADED_CALLBACK_ADMISSION_DEPTH: std::cell::Cell<u32> = std::cell::Cell::new(0);
}

#[derive(Debug, Default)]
struct ThreadedCallbackAdmission {
    state: CallbackMutex<ThreadedCallbackAdmissionState>,
    drained: CallbackCondvar,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ThreadedCallbackAdmissionPhase {
    /// No callback can enter and every previously admitted callback has returned.
    #[default]
    Closed,
    /// New callbacks may enter.
    Open,
    /// Extension teardown has rejected new worker callbacks. Existing workers may still be draining, while callbacks on the recorded main
    /// thread remain legal until final binding teardown.
    MainThreadOnly,
    /// Final binding teardown rejects every new callback and is waiting for existing main-thread leases to return.
    Closing,
}

#[derive(Debug, Default)]
struct ThreadedCallbackAdmissionState {
    phase: ThreadedCallbackAdmissionPhase,
    active: usize,
    main_thread: Option<CallbackThreadId>,
}

impl ThreadedCallbackAdmission {
    fn lock_state(&self) -> CallbackMutexGuard<'_, ThreadedCallbackAdmissionState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn open(&self) {
        let mut state = self.lock_state();
        assert_eq!(
            state.phase,
            ThreadedCallbackAdmissionPhase::Closed,
            "threaded callback admission opened before the previous teardown completed"
        );
        assert_eq!(
            state.active, 0,
            "threaded callback admission reopened with active callbacks"
        );
        state.phase = ThreadedCallbackAdmissionPhase::Open;
        state.main_thread = Some(current_callback_thread_id());
    }

    fn ensure_open_for_level_init(&self) {
        let mut state = self.lock_state();
        match state.phase {
            // Ordinary startup invokes one callback per initialization level after low-level binding initialization has already opened the
            // gate. Those later levels are intentionally idempotent.
            ThreadedCallbackAdmissionPhase::Open => {}
            // Godot can deinitialize levels and later reinitialize Scene+ without a fresh low-level binding initialization (gdext#629).
            // The preceding close drained every lease, so the first returning level starts a new callback-admission cycle.
            ThreadedCallbackAdmissionPhase::Closed => {
                assert_eq!(
                    state.active, 0,
                    "threaded callback admission reopened with active callbacks"
                );
                state.phase = ThreadedCallbackAdmissionPhase::Open;
                state.main_thread = Some(current_callback_thread_id());
            }
            ThreadedCallbackAdmissionPhase::MainThreadOnly => {
                assert_eq!(
                    state.active, 0,
                    "initialization level resumed before worker callbacks drained"
                );
                state.phase = ThreadedCallbackAdmissionPhase::Open;
                state.main_thread = Some(current_callback_thread_id());
            }
            ThreadedCallbackAdmissionPhase::Closing => {
                panic!("initialization level resumed before threaded callback teardown drained")
            }
        }
    }

    fn update_main_thread(&self) {
        let mut state = self.lock_state();
        assert_eq!(
            state.phase,
            ThreadedCallbackAdmissionPhase::Open,
            "main-thread identity changed outside extension initialization"
        );
        state.main_thread = Some(current_callback_thread_id());
    }

    fn enter(&self) -> ThreadedCallbackLease<'_> {
        self.try_enter()
            .expect("threaded Godot callback started while the binding was not accepting callbacks")
    }

    fn try_enter(&self) -> Option<ThreadedCallbackLease<'_>> {
        let mut state = self.lock_state();
        match state.phase {
            ThreadedCallbackAdmissionPhase::Open => {}
            ThreadedCallbackAdmissionPhase::MainThreadOnly
                if state.main_thread == Some(current_callback_thread_id()) => {}
            ThreadedCallbackAdmissionPhase::MainThreadOnly
            | ThreadedCallbackAdmissionPhase::Closing
            | ThreadedCallbackAdmissionPhase::Closed => return None,
        }

        state.active = state
            .active
            .checked_add(1)
            .expect("threaded callback lease count overflow");
        Some(ThreadedCallbackLease { admission: self })
    }

    fn close_workers_and_wait(&self) {
        let mut state = self.lock_state();
        match state.phase {
            ThreadedCallbackAdmissionPhase::Open => {
                state.phase = ThreadedCallbackAdmissionPhase::MainThreadOnly;
                // Wake tests and any second teardown caller waiting for the phase transition.
                self.drained.notify_all();
            }
            // The earliest high-level teardown boundary closes the gate. Later init-level callbacks and low-level binding deinitialization
            // deliberately repeat the operation, so closing must be idempotent.
            ThreadedCallbackAdmissionPhase::MainThreadOnly
            | ThreadedCallbackAdmissionPhase::Closing
            | ThreadedCallbackAdmissionPhase::Closed => {}
        }

        while state.active != 0 {
            state = self
                .drained
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    fn close_all_and_wait(&self) {
        let mut state = self.lock_state();
        match state.phase {
            ThreadedCallbackAdmissionPhase::Open
            | ThreadedCallbackAdmissionPhase::MainThreadOnly => {
                state.phase = ThreadedCallbackAdmissionPhase::Closing;
                self.drained.notify_all();
            }
            ThreadedCallbackAdmissionPhase::Closing | ThreadedCallbackAdmissionPhase::Closed => {}
        }

        while state.active != 0 {
            state = self
                .drained
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }

        state.phase = ThreadedCallbackAdmissionPhase::Closed;
        state.main_thread = None;
        self.drained.notify_all();
    }

    #[cfg(test)]
    fn close_and_wait(&self) {
        self.close_all_and_wait();
    }

    fn assert_closed_and_drained(&self) {
        let state = self.lock_state();
        assert_eq!(
            state.phase,
            ThreadedCallbackAdmissionPhase::Closed,
            "binding deinitialization reached storage clearing before threaded callbacks were closed and drained"
        );
        assert_eq!(
            state.active, 0,
            "binding deinitialization reached storage clearing with active threaded callbacks"
        );
        assert!(
            state.main_thread.is_none(),
            "binding deinitialization retained a stale callback main-thread identity"
        );
    }

    #[cfg(test)]
    fn wait_until_not_accepting(&self) {
        let mut state = self.lock_state();
        while state.phase == ThreadedCallbackAdmissionPhase::Open {
            state = self
                .drained
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }
}

struct ThreadedCallbackLease<'a> {
    admission: &'a ThreadedCallbackAdmission,
}

impl Drop for ThreadedCallbackLease<'_> {
    fn drop(&mut self) {
        let mut state = self.admission.lock_state();
        state.active = state
            .active
            .checked_sub(1)
            .expect("threaded callback lease released more often than acquired");
        if state.active == 0 {
            self.admission.drained.notify_all();
        }
    }
}

struct ThreadedCallbackAdmissionGuard {
    _not_send_sync: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl ThreadedCallbackAdmissionGuard {
    fn enter() -> Self {
        THREADED_CALLBACK_ADMISSION_DEPTH.with(|depth| {
            depth.set(
                depth
                    .get()
                    .checked_add(1)
                    .expect("threaded callback admission nesting overflow"),
            );
        });

        Self {
            _not_send_sync: std::marker::PhantomData,
        }
    }
}

impl Drop for ThreadedCallbackAdmissionGuard {
    fn drop(&mut self) {
        THREADED_CALLBACK_ADMISSION_DEPTH.with(|depth| {
            let current = depth.get();
            debug_assert!(current > 0, "unbalanced threaded callback admission scope");
            depth.set(current - 1);
        });
    }
}

fn is_in_threaded_callback_admission() -> bool {
    THREADED_CALLBACK_ADMISSION_DEPTH.with(|depth| depth.get() != 0)
}

fn threaded_callback_admission() -> &'static ThreadedCallbackAdmission {
    static ADMISSION: std::sync::OnceLock<ThreadedCallbackAdmission> = std::sync::OnceLock::new();
    ADMISSION.get_or_init(ThreadedCallbackAdmission::default)
}

pub(super) fn open_threaded_callback_admission() {
    threaded_callback_admission().open();
}

/// Keeps callback admission open during ordinary multi-level initialization, or reopens it after Godot partially unloaded and then resumed
/// initialization levels without recreating the low-level binding.
///
/// # Safety
///
/// Must be called on the engine initialization thread before internal or user initialization for the returning level. A prior teardown must
/// have completely drained before Godot begins reinitialization.
#[doc(hidden)]
pub unsafe fn resume_threaded_callback_admission_for_level_init() {
    threaded_callback_admission().ensure_open_for_level_init();
}

/// Refreshes the main-thread identity used to admit synchronous teardown callbacks.
///
/// # Safety
///
/// Must be called during exclusive extension initialization, after the engine has moved initialization to the new main thread and before any
/// user callback can run there.
#[doc(hidden)]
pub unsafe fn update_threaded_callback_main_thread() {
    threaded_callback_admission().update_main_thread();
}

pub(super) fn assert_threaded_callback_teardown_complete() {
    threaded_callback_admission().assert_closed_and_drained();
}

/// Stops new worker-thread callbacks and waits for every previously admitted trampoline to return.
///
/// This is idempotent within one binding lifetime: the MainLoop shutdown callback, each later initialization-level deinitializer, and final
/// low-level binding teardown may all call it. The first caller linearizes worker teardown; later callers only verify or wait for the same
/// drain. Synchronous callbacks on the recorded main thread remain legal for user deinitializers and class cleanup until
/// [`finish_threaded_callback_teardown`] closes the final window. A subsequent successful binding initialization opens a fresh cycle.
///
/// # Safety
///
/// Must be called on the engine's deinitialization thread at the first extension teardown boundary, before user teardown code or callback
/// userdata can be destroyed. The engine must not require a newly dispatched explicitly threaded callback to complete teardown after this
/// boundary; such a late worker callback is rejected fail-stop.
#[doc(hidden)]
pub unsafe fn begin_threaded_callback_teardown() {
    begin_threaded_callback_teardown_in(threaded_callback_admission());
}

fn begin_threaded_callback_teardown_in(admission: &ThreadedCallbackAdmission) {
    assert!(
        !is_in_threaded_callback_admission(),
        "binding teardown cannot run from inside an admitted threaded callback"
    );
    admission.close_workers_and_wait();
}

/// Rejects every new callback and drains any remaining main-thread callback before binding storage is cleared.
///
/// # Safety
///
/// Must be called on the engine's final deinitialization thread after all user teardown and callback-userdata destruction has completed.
#[doc(hidden)]
pub unsafe fn finish_threaded_callback_teardown() {
    close_threaded_callback_admission_and_wait_in(threaded_callback_admission());
}

fn close_threaded_callback_admission_and_wait_in(admission: &ThreadedCallbackAdmission) {
    assert!(
        !is_in_threaded_callback_admission(),
        "binding teardown cannot run from inside an admitted threaded callback"
    );
    admission.close_all_and_wait();
}

/// Whether scoped worker callbacks can use eagerly initialized binding tables in this build.
///
/// Lazy function-pointer initialization is not synchronized and therefore cannot be enabled from a worker callback.
#[doc(hidden)]
pub const UNSAFE_THREADED_CALLBACKS_SUPPORTED: bool = !cfg!(feature = "codegen-lazy-fptrs");

/// Scope guard used by an explicitly opted-in Godot -> Rust worker callback.
///
/// This is deliberately not a general public threading API. It narrows the behavior of `experimental-threads` to the dynamic extent of one
/// explicitly opted-in interface virtual. Lifecycle, property and exported method callbacks do not enter this scope. The caller remains
/// responsible for using Godot APIs that are valid in that callback's thread context. Same-thread synchronous callbacks re-entered through
/// Godot inherit the current thread-local scope; the unsafe opt-in must cover that complete path.
struct ThreadedCallbackGuard {
    // The permission belongs to this OS thread. Moving the guard would decrement another thread's depth on drop and leave the origin enabled.
    _not_send_sync: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl ThreadedCallbackGuard {
    /// Lets the current callback use the binding from a Godot worker thread.
    ///
    /// # Safety
    /// Godot must have invoked the callback in a context where every FFI operation performed by the callback is thread-safe. This guard does
    /// not make scene-tree, object, or arbitrary extension API access thread-safe. The binding must use eagerly initialized function tables;
    /// lazy function-pointer initialization is not synchronized across worker threads.
    unsafe fn enter() -> Self {
        if !UNSAFE_THREADED_CALLBACKS_SUPPORTED {
            panic!("threaded callback scopes require eagerly initialized function tables");
        }

        THREADED_CALLBACK_DEPTH.with(|depth| {
            depth.set(
                depth
                    .get()
                    .checked_add(1)
                    .expect("threaded callback nesting overflow"),
            );
        });

        Self {
            _not_send_sync: std::marker::PhantomData,
        }
    }
}

/// Proof that an explicitly opted-in worker callback holds a binding-lifetime lease.
///
/// Values are created only by [`with_threaded_callback_admission`]. Private fields prevent downstream macro expansions from constructing a
/// token without first entering the admission gate.
#[doc(hidden)]
pub struct ThreadedCallbackToken<'a> {
    _lease: Option<ThreadedCallbackLease<'a>>,
    _not_send_sync: std::marker::PhantomData<(&'a (), std::rc::Rc<()>)>,
}

/// Runs the complete generated trampoline while holding a binding-lifetime lease.
///
/// The scope includes panic/error conversion after the user method returns because that code can log through the Godot binding. Same-thread
/// synchronous re-entry while an admitted trampoline is active reuses the outer lease, which remains alive for the complete nested call stack.
/// A callback dispatched on another thread is a new admission and must begin before extension teardown closes worker admission. Main-thread
/// callbacks remain admissible for synchronous user/class cleanup until final binding teardown. A callback which violates that shutdown
/// contract is rejected with a fail-stop panic before its body starts; generated `extern "C"` trampolines cannot turn such an engine lifecycle
/// violation into a generally valid ABI return value.
///
/// # Safety
/// `callback` must be the complete synchronous body of a Godot callback covered by an explicit unsafe threaded class/interface contract. A
/// callback starting on a different thread is a new admission and must enter before extension teardown closes worker admission.
#[doc(hidden)]
pub unsafe fn with_threaded_callback_admission<R>(
    callback: impl FnOnce(&ThreadedCallbackToken<'_>) -> R,
) -> R {
    if !UNSAFE_THREADED_CALLBACKS_SUPPORTED {
        panic!("threaded callback scopes require eagerly initialized function tables");
    }

    // SAFETY: this function's contract is forwarded unchanged to the implementation using the process-global admission gate. The gate is
    // required even when `experimental-threads` permits general cross-thread binding access: explicit callback opt-ins still promise that
    // their complete trampoline pins the binding through teardown.
    unsafe { with_threaded_callback_admission_in(threaded_callback_admission(), callback) }
}

/// Holds the binding-lifetime admission lease across an ordinary generated callback belonging to
/// an explicitly threaded class.
///
/// Ordinary classes bypass the gate entirely. An explicitly threaded class still needs the lease:
/// blocking instance storage makes a claim possible on any thread, and deinitialization must not
/// clear the binding while an ordinary main-thread callback or a valid synchronous re-entry is
/// active.
///
/// # Safety
/// When `explicitly_threaded_class` is true, `callback` must be the complete generated trampoline
/// and its first operation must call [`ensure_ordinary_callback_thread`] before touching the
/// binding, instance storage, ABI arguments, or user code.
#[doc(hidden)]
#[inline(always)]
pub unsafe fn with_ordinary_callback_admission<R>(
    explicitly_threaded_class: bool,
    callback: impl FnOnce() -> R,
) -> R {
    if explicitly_threaded_class {
        // SAFETY: forwarded from this function's contract. Ordinary callbacks do not enter the
        // worker binding-permission scope; this admission only pins binding lifetime.
        unsafe {
            with_threaded_callback_admission_in(threaded_callback_admission(), |_| callback())
        }
    } else {
        callback()
    }
}

unsafe fn with_threaded_callback_admission_in<'a, R>(
    admission: &'a ThreadedCallbackAdmission,
    callback: impl FnOnce(&ThreadedCallbackToken<'a>) -> R,
) -> R {
    let lease = if is_in_threaded_callback_admission() {
        // The outer token cannot drop before a synchronous nested callback returns. Reuse its lease even after the narrower FFI-permission
        // scope has ended, such as during panic conversion or Godot error logging.
        None
    } else {
        Some(admission.enter())
    };

    let token = ThreadedCallbackToken {
        _lease: lease,
        _not_send_sync: std::marker::PhantomData,
    };
    // Created after `token`, so this depth is revoked before the outermost token releases its lease during return or unwinding.
    let _admission_guard = ThreadedCallbackAdmissionGuard::enter();
    callback(&token)
}

/// Runs the user invocation and generated marshalling with worker-thread binding permission.
///
/// Requiring a non-forgeable admission token keeps the permission within a callback whose binding lifetime is pinned. The private guard makes
/// the permission structurally scoped: downstream code cannot leak it with `mem::forget`.
///
/// # Safety
/// Every FFI operation performed by `callback`, including synchronous re-entry, must be valid on the current worker thread. This does not make
/// scene-tree, object, or arbitrary extension API access thread-safe.
#[doc(hidden)]
pub unsafe fn with_threaded_callback_scope<R>(
    _token: &ThreadedCallbackToken<'_>,
    callback: impl FnOnce() -> R,
) -> R {
    // SAFETY: delegated to this function's caller. The private guard drops on normal return and during unwinding.
    let _guard = unsafe { ThreadedCallbackGuard::enter() };
    callback()
}

impl Drop for ThreadedCallbackGuard {
    fn drop(&mut self) {
        THREADED_CALLBACK_DEPTH.with(|depth| {
            let current = depth.get();
            debug_assert!(current > 0, "unbalanced threaded callback scope");
            depth.set(current - 1);
        });
    }
}

fn is_in_threaded_callback() -> bool {
    THREADED_CALLBACK_DEPTH.with(|depth| depth.get() != 0)
}

/// Rejects an ordinary generated callback for an explicitly threaded class before it can claim
/// instance storage, unmarshal arguments, or run user code on an unapproved worker thread.
///
/// A synchronous ordinary callback re-entered from the dynamic scope of an already selected
/// threaded virtual is allowed on that same OS thread. Its complete path is covered by the outer
/// callback's unsafe contract and inherits the outer thread-local binding permission. Merely
/// holding a binding-lifetime admission lease is not enough: after the selected invocation scope
/// ends, ordinary callbacks return to their main-thread-only contract.
///
/// This boundary is deliberately independent of safeguard level. In particular, release builds
/// with disengaged safeguards must fail before entering the callback rather than relying on a
/// later checked Godot API access which may not exist in a primitive-only callback body.
#[doc(hidden)]
#[inline(always)]
pub fn ensure_ordinary_callback_thread(explicitly_threaded_class: bool) {
    if explicitly_threaded_class {
        ensure_ordinary_callback_thread_in(crate::is_main_thread());
    }
}

#[inline(always)]
fn ensure_ordinary_callback_thread_in(is_main_thread: bool) {
    if !is_main_thread && !is_in_threaded_callback() {
        panic!(
            "ordinary callback for #[class(unsafe_threaded)] started outside the main thread and outside a synchronous opted-in threaded callback"
        );
    }
}

#[cfg(all(test, not(loom), not(feature = "codegen-lazy-fptrs")))]
mod ordinary_callback_thread_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        ThreadedCallbackAdmission, ensure_ordinary_callback_thread_in,
        with_threaded_callback_admission_in, with_threaded_callback_scope,
    };

    unsafe fn with_local_ordinary_admission<R>(
        admission: &ThreadedCallbackAdmission,
        explicitly_threaded_class: bool,
        callback: impl FnOnce() -> R,
    ) -> R {
        if explicitly_threaded_class {
            // SAFETY: test-only equivalent of the generated ordinary trampoline. Its caller
            // places the thread gate first in `callback`.
            unsafe { with_threaded_callback_admission_in(admission, |_| callback()) }
        } else {
            callback()
        }
    }

    unsafe fn run_ordinary_primitive_callback(
        admission: &ThreadedCallbackAdmission,
        explicitly_threaded_class: bool,
        is_main_thread: bool,
        body: impl FnOnce(),
    ) {
        // SAFETY: the thread gate is the first operation in the complete modeled trampoline.
        unsafe {
            with_local_ordinary_admission(admission, explicitly_threaded_class, || {
                if explicitly_threaded_class {
                    ensure_ordinary_callback_thread_in(is_main_thread);
                }
                body();
            })
        }
    }

    #[test]
    fn worker_is_rejected_before_ordinary_primitive_abi_body() {
        let admission = Arc::new(ThreadedCallbackAdmission::default());
        admission.open();
        let body_calls = Arc::new(AtomicUsize::new(0));
        let worker_admission = Arc::clone(&admission);
        let worker_body_calls = Arc::clone(&body_calls);

        let result = std::thread::spawn(move || {
            std::panic::catch_unwind(|| {
                // SAFETY: this models the generated trampoline and puts the gate before the body.
                unsafe {
                    run_ordinary_primitive_callback(&worker_admission, true, false, || {
                        worker_body_calls.fetch_add(1, Ordering::SeqCst);
                    });
                }
            })
        })
        .join()
        .unwrap();

        assert!(result.is_err());
        assert_eq!(body_calls.load(Ordering::SeqCst), 0);
        assert_eq!(admission.lock_state().active, 0);
        admission.close_and_wait();
    }

    #[test]
    fn ordinary_callback_gate_preserves_main_thread_and_unopted_classes() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();
        let body_calls = AtomicUsize::new(0);

        unsafe {
            run_ordinary_primitive_callback(&admission, true, true, || {
                body_calls.fetch_add(1, Ordering::SeqCst);
            });
            run_ordinary_primitive_callback(&admission, false, false, || {
                body_calls.fetch_add(1, Ordering::SeqCst);
            });
        }

        assert_eq!(body_calls.load(Ordering::SeqCst), 2);
        admission.close_and_wait();
    }

    #[test]
    fn ordinary_callback_pins_binding_until_complete() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);

        std::thread::scope(|scope| {
            let admission_ref = &admission;
            let callback = scope.spawn(move || unsafe {
                run_ordinary_primitive_callback(admission_ref, true, true, || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                });
            });

            entered_rx.recv().unwrap();
            let close = scope.spawn(|| admission.close_and_wait());
            admission.wait_until_not_accepting();
            assert!(!close.is_finished());

            release_tx.send(()).unwrap();
            callback.join().unwrap();
            close.join().unwrap();
        });

        assert_eq!(admission.lock_state().active, 0);
    }

    #[test]
    fn synchronous_reentry_is_allowed_only_inside_selected_callback_scope() {
        let admission = Arc::new(ThreadedCallbackAdmission::default());
        admission.open();
        let body_calls = Arc::new(AtomicUsize::new(0));

        let worker_admission = Arc::clone(&admission);
        let worker_body_calls = Arc::clone(&body_calls);
        let after_scope_result = std::thread::spawn(move || unsafe {
            with_threaded_callback_admission_in(&worker_admission, |token| {
                with_threaded_callback_scope(token, || {
                    run_ordinary_primitive_callback(&worker_admission, true, false, || {
                        worker_body_calls.fetch_add(1, Ordering::SeqCst);
                    });
                });

                // A binding-lifetime lease by itself does not extend the unsafe permission into
                // panic conversion or other post-invocation work.
                std::panic::catch_unwind(|| {
                    run_ordinary_primitive_callback(&worker_admission, true, false, || {
                        worker_body_calls.fetch_add(1, Ordering::SeqCst);
                    });
                })
            })
        })
        .join()
        .unwrap();

        assert!(after_scope_result.is_err());
        assert_eq!(body_calls.load(Ordering::SeqCst), 1);
        admission.close_and_wait();
    }
}

#[cfg(feature = "experimental-threads")]
mod multi_threaded;
#[cfg(not(feature = "experimental-threads"))]
mod single_threaded;

#[cfg(feature = "experimental-threads")]
use multi_threaded::BindingStorage;
#[cfg(not(feature = "experimental-threads"))]
use single_threaded::BindingStorage;

pub struct GdextConfig {
    /// True if only `#[class(tool)]` classes are active in editor; false if all classes are.
    pub tool_only_in_editor: bool,
}

impl GdextConfig {
    pub fn new(tool_only_in_editor: bool) -> Self {
        Self {
            tool_only_in_editor,
        }
    }
}

/// Panics if the binding is not currently live. Shared by both binding storages for a consistent live check.
#[cfg(safeguards_balanced)]
#[inline(always)]
pub(super) fn assert_binding_live(initialized: &std::sync::atomic::AtomicBool) {
    if !initialized.load(std::sync::atomic::Ordering::Acquire) {
        not_live_panic();
    }
}

/// Cold failure path, so the hot check stays a predicted-not-taken branch.
#[cfg(safeguards_balanced)]
#[cold]
#[inline(never)]
fn not_live_panic() -> ! {
    panic!(
        "Godot binding accessed before initialization or after deinitialization. \
        This typically means a `#[ctor]`/`#[dtor]` constructor, a library destructor, or a leftover user thread touched the Godot API \
        outside the engine's load/unload window."
    )
}

#[cfg(all(test, safeguards_balanced))]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::assert_binding_live;

    #[test]
    fn live_check_passes_when_initialized() {
        // Must not panic.
        assert_binding_live(&AtomicBool::new(true));
    }

    #[test]
    #[should_panic(expected = "accessed before initialization or after deinitialization")]
    fn live_check_panics_when_not_initialized() {
        assert_binding_live(&AtomicBool::new(false));
    }
}

#[cfg(all(
    test,
    not(loom),
    not(feature = "experimental-threads"),
    not(feature = "codegen-lazy-fptrs")
))]
mod threaded_callback_tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};

    use super::{
        ThreadedCallbackAdmission, ThreadedCallbackAdmissionPhase, ThreadedCallbackToken,
        begin_threaded_callback_teardown_in, close_threaded_callback_admission_and_wait_in,
        is_in_threaded_callback, with_threaded_callback_admission_in, with_threaded_callback_scope,
    };

    unsafe fn with_local_admission<R>(
        admission: &ThreadedCallbackAdmission,
        callback: impl FnOnce(&ThreadedCallbackToken<'_>) -> R,
    ) -> R {
        // SAFETY: test-only forwarding to a local gate with the same callback contract.
        unsafe { with_threaded_callback_admission_in(admission, callback) }
    }

    #[test]
    fn threaded_callback_scope_is_nested_and_thread_local() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();
        assert!(!is_in_threaded_callback());

        unsafe {
            with_local_admission(&admission, |token| {
                with_threaded_callback_scope(token, || {
                    assert!(is_in_threaded_callback());

                    with_threaded_callback_scope(token, || {
                        assert!(is_in_threaded_callback());
                    });
                    assert!(is_in_threaded_callback());

                    let barrier = Arc::new(Barrier::new(2));
                    let worker_barrier = barrier.clone();
                    let worker = std::thread::spawn(move || {
                        assert!(!is_in_threaded_callback());
                        worker_barrier.wait();
                        assert!(!is_in_threaded_callback());
                    });
                    barrier.wait();
                    worker.join().unwrap();
                })
            });
        }
        assert!(!is_in_threaded_callback());
        admission.close_and_wait();
    }

    #[test]
    fn threaded_callback_scope_is_revoked_during_unwind() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();
        let result = std::panic::catch_unwind(|| unsafe {
            with_local_admission(&admission, |token| {
                with_threaded_callback_scope(token, || {
                    assert!(is_in_threaded_callback());
                    panic!("scope test");
                })
            });
        });

        assert!(result.is_err());
        assert!(!is_in_threaded_callback());
        admission.close_and_wait();
    }

    #[test]
    fn binding_close_waits_for_admitted_callback() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();
        let lease = admission.enter();

        std::thread::scope(|scope| {
            let close = scope.spawn(|| admission.close_and_wait());
            admission.wait_until_not_accepting();
            assert!(!close.is_finished());

            drop(lease);
            close.join().unwrap();
        });
    }

    #[test]
    fn binding_close_waits_for_every_concurrent_callback() {
        const CALLBACKS: usize = 16;

        let admission = ThreadedCallbackAdmission::default();
        admission.open();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);

        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..CALLBACKS)
                .map(|_| {
                    let entered_tx = entered_tx.clone();
                    let release_rx = &release_rx;
                    let admission = &admission;
                    scope.spawn(move || unsafe {
                        with_local_admission(admission, |_| {
                            entered_tx.send(()).unwrap();
                            release_rx.lock().unwrap().recv().unwrap();
                        });
                    })
                })
                .collect();
            drop(entered_tx);

            for _ in 0..CALLBACKS {
                entered_rx.recv().unwrap();
            }

            let close = scope.spawn(|| admission.close_and_wait());
            admission.wait_until_not_accepting();
            assert!(!close.is_finished());

            for _ in 1..CALLBACKS {
                release_tx.send(()).unwrap();
            }
            loop {
                let state = admission.lock_state();
                if state.active == 1 {
                    break;
                }
                drop(state);
                std::thread::yield_now();
            }
            assert!(!close.is_finished());

            release_tx.send(()).unwrap();
            for worker in workers {
                worker.join().unwrap();
            }
            close.join().unwrap();
        });

        let state = admission.lock_state();
        assert_eq!(state.phase, ThreadedCallbackAdmissionPhase::Closed);
        assert_eq!(state.active, 0);
    }

    #[test]
    fn teardown_from_inside_callback_is_rejected_without_closing_gate() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();

        unsafe {
            with_local_admission(&admission, |_| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    close_threaded_callback_admission_and_wait_in(&admission);
                }));
                assert!(result.is_err());

                let state = admission.lock_state();
                assert_eq!(state.phase, ThreadedCallbackAdmissionPhase::Open);
                assert_eq!(state.active, 1);
            });
        }

        admission.close_and_wait();
    }

    #[test]
    fn binding_close_waits_through_post_invocation_work() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();
        let (invocation_done_tx, invocation_done_rx) = std::sync::mpsc::sync_channel(0);
        let (finish_tx, finish_rx) = std::sync::mpsc::sync_channel(0);

        std::thread::scope(|scope| {
            let admission_ref = &admission;
            let callback = scope.spawn(move || unsafe {
                with_local_admission(admission_ref, |token| {
                    with_threaded_callback_scope(token, || {
                        assert!(is_in_threaded_callback());
                    });

                    // Models panic conversion/error logging after the user invocation: worker permission is gone, but the binding lease must
                    // remain active until the complete trampoline returns.
                    assert!(!is_in_threaded_callback());
                    invocation_done_tx.send(()).unwrap();
                    finish_rx.recv().unwrap();

                    // Admission is now closed, but synchronous re-entry must reuse the outer lease rather than trying to enter the gate.
                    with_threaded_callback_admission_in(admission_ref, |_| {});
                });
            });

            invocation_done_rx.recv().unwrap();
            let close = scope.spawn(|| admission.close_and_wait());
            admission.wait_until_not_accepting();
            assert!(!close.is_finished());

            finish_tx.send(()).unwrap();
            callback.join().unwrap();
            close.join().unwrap();
        });
    }

    #[test]
    fn close_drain_rejects_new_threads_but_preserves_synchronous_reentry() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();
        let late_body_ran = AtomicBool::new(false);
        let (outer_entered_tx, outer_entered_rx) = std::sync::mpsc::sync_channel(0);
        let (reenter_tx, reenter_rx) = std::sync::mpsc::sync_channel(0);

        std::thread::scope(|scope| {
            let admission_ref = &admission;
            let outer = scope.spawn(move || unsafe {
                with_local_admission(admission_ref, |_| {
                    outer_entered_tx.send(()).unwrap();
                    reenter_rx.recv().unwrap();

                    // The complete outer trampoline still owns the one admitted lease. A nested callback on this same thread is part of
                    // that synchronous call stack and must therefore remain valid after close has stopped new threads.
                    with_threaded_callback_admission_in(admission_ref, |_| {});
                });
            });

            outer_entered_rx.recv().unwrap();
            let close = scope.spawn(|| admission.close_and_wait());
            admission.wait_until_not_accepting();
            assert!(!close.is_finished());

            // This models an engine worker which reaches a callback only after teardown has linearized. Its body must never run and its
            // failed attempt must neither revive nor decrement the already-active outer lease.
            let late_body_ran_ref = &late_body_ran;
            let late = scope.spawn(move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                    with_local_admission(admission_ref, |_| {
                        late_body_ran_ref.store(true, Ordering::SeqCst);
                    });
                }))
            });
            assert!(late.join().unwrap().is_err());
            assert!(!late_body_ran.load(Ordering::SeqCst));
            {
                let state = admission.lock_state();
                assert_eq!(state.phase, ThreadedCallbackAdmissionPhase::Closing);
                assert_eq!(state.active, 1);
            }
            assert!(!close.is_finished());

            reenter_tx.send(()).unwrap();
            outer.join().unwrap();
            close.join().unwrap();
        });

        let state = admission.lock_state();
        assert_eq!(state.phase, ThreadedCallbackAdmissionPhase::Closed);
        assert_eq!(state.active, 0);
    }

    #[test]
    fn admission_can_reopen_only_after_a_complete_close() {
        let admission = ThreadedCallbackAdmission::default();

        for _ in 0..3 {
            admission.open();
            unsafe { with_local_admission(&admission, |_| {}) };
            admission.close_and_wait();
        }
    }

    #[test]
    fn earliest_teardown_boundary_drains_before_user_state_destruction_and_rejects_late_callbacks()
    {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();

        let user_state_destroyed = AtomicBool::new(false);
        let late_body_ran = AtomicBool::new(false);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);

        std::thread::scope(|scope| {
            let admission_ref = &admission;
            let user_state_destroyed_ref = &user_state_destroyed;
            let callback = scope.spawn(move || unsafe {
                with_local_admission(admission_ref, |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    assert!(!user_state_destroyed_ref.load(Ordering::SeqCst));
                });
            });
            entered_rx.recv().unwrap();

            let admission_ref = &admission;
            let user_state_destroyed_ref = &user_state_destroyed;
            let teardown = scope.spawn(move || {
                begin_threaded_callback_teardown_in(admission_ref);
                // Models the first user deinitializer / class-unregistration action after the boundary helper returns.
                user_state_destroyed_ref.store(true, Ordering::SeqCst);
            });

            admission.wait_until_not_accepting();
            assert!(!teardown.is_finished());
            assert!(!user_state_destroyed.load(Ordering::SeqCst));

            let admission_ref = &admission;
            let late_body_ran_ref = &late_body_ran;
            let late = scope.spawn(move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                    with_local_admission(admission_ref, |_| {
                        late_body_ran_ref.store(true, Ordering::SeqCst);
                    });
                }))
            });
            assert!(late.join().unwrap().is_err());
            assert!(!late_body_ran.load(Ordering::SeqCst));
            assert!(!user_state_destroyed.load(Ordering::SeqCst));

            release_tx.send(()).unwrap();
            callback.join().unwrap();
            teardown.join().unwrap();
        });

        assert!(user_state_destroyed.load(Ordering::SeqCst));
        {
            let state = admission.lock_state();
            assert_eq!(state.phase, ThreadedCallbackAdmissionPhase::MainThreadOnly);
            assert_eq!(state.active, 0);
        }

        // User/class teardown can still synchronously invoke a blocking callback on the main thread.
        unsafe { with_local_admission(&admission, |_| {}) };

        // A worker remains rejected throughout the main-thread teardown window.
        let late_worker_body_ran = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let late_worker_body_ran_ref = &late_worker_body_ran;
            let admission_ref = &admission;
            let late_worker = scope.spawn(move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                    with_local_admission(admission_ref, |_| {
                        late_worker_body_ran_ref.store(true, Ordering::SeqCst);
                    });
                }))
            });
            assert!(late_worker.join().unwrap().is_err());
        });
        assert!(!late_worker_body_ran.load(Ordering::SeqCst));

        // Later init-level boundaries repeat the worker close without a double-close failure. Final low-level teardown then closes main too.
        begin_threaded_callback_teardown_in(&admission);
        close_threaded_callback_admission_and_wait_in(&admission);
        admission.assert_closed_and_drained();
    }

    #[test]
    fn partial_level_reinit_reopens_after_complete_drain() {
        let admission = ThreadedCallbackAdmission::default();

        // Full binding init, followed by idempotent Core/Servers/Scene/Editor level init callbacks.
        admission.open();
        for _ in 0..4 {
            admission.ensure_open_for_level_init();
        }
        unsafe { with_local_admission(&admission, |_| {}) };

        // A partial level unload closes the gate even though the low-level binding remains alive.
        begin_threaded_callback_teardown_in(&admission);
        unsafe { with_local_admission(&admission, |_| {}) };
        std::thread::scope(|scope| {
            let admission_ref = &admission;
            assert!(
                scope
                    .spawn(move || admission_ref.try_enter().is_none())
                    .join()
                    .unwrap()
            );
        });

        // The first resumed level opens a fresh cycle; subsequent level callbacks remain idempotent.
        admission.ensure_open_for_level_init();
        admission.ensure_open_for_level_init();
        unsafe { with_local_admission(&admission, |_| {}) };

        admission.close_and_wait();
        admission.assert_closed_and_drained();
    }

    #[test]
    fn main_thread_identity_refresh_controls_teardown_admission() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();
        let (updated_tx, updated_rx) = std::sync::mpsc::sync_channel(0);
        let (continue_tx, continue_rx) = std::sync::mpsc::sync_channel(0);
        let new_main_body_ran = AtomicBool::new(false);

        std::thread::scope(|scope| {
            let admission_ref = &admission;
            let new_main_body_ran_ref = &new_main_body_ran;
            let new_main = scope.spawn(move || {
                // Models Android moving initialization to another main thread at Servers level.
                admission_ref.update_main_thread();
                updated_tx.send(()).unwrap();
                continue_rx.recv().unwrap();

                unsafe {
                    with_local_admission(admission_ref, |_| {
                        new_main_body_ran_ref.store(true, Ordering::SeqCst);
                    });
                }
            });

            updated_rx.recv().unwrap();
            begin_threaded_callback_teardown_in(&admission);

            // The former main thread is now treated as a worker during teardown.
            let former_main = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                with_local_admission(&admission, |_| {});
            }));
            assert!(former_main.is_err());

            continue_tx.send(()).unwrap();
            new_main.join().unwrap();
        });

        assert!(new_main_body_ran.load(Ordering::SeqCst));
        admission.close_and_wait();
        admission.assert_closed_and_drained();
    }

    #[test]
    fn late_admission_is_rejected_without_running_callback_or_reviving_count() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();
        admission.close_and_wait();
        let body_ran = AtomicBool::new(false);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            with_local_admission(&admission, |_| body_ran.store(true, Ordering::SeqCst));
        }));

        assert!(result.is_err());
        assert!(!body_ran.load(Ordering::SeqCst));
        let state = admission.lock_state();
        assert_eq!(state.phase, ThreadedCallbackAdmissionPhase::Closed);
        assert_eq!(state.active, 0);
    }
}

#[cfg(all(
    test,
    not(loom),
    feature = "experimental-threads",
    not(feature = "codegen-lazy-fptrs")
))]
mod experimental_threaded_callback_admission_tests {
    use super::{
        ThreadedCallbackAdmission, ThreadedCallbackToken, with_threaded_callback_admission_in,
    };

    unsafe fn with_local_admission<R>(
        admission: &ThreadedCallbackAdmission,
        callback: impl FnOnce(&ThreadedCallbackToken<'_>) -> R,
    ) -> R {
        // SAFETY: test-only forwarding to a local gate with the same callback contract.
        unsafe { with_threaded_callback_admission_in(admission, callback) }
    }

    #[test]
    fn experimental_mode_close_waits_for_admitted_callback() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();
        let lease = admission.enter();

        std::thread::scope(|scope| {
            let close = scope.spawn(|| admission.close_and_wait());
            admission.wait_until_not_accepting();
            assert!(!close.is_finished());

            drop(lease);
            close.join().unwrap();
        });
    }

    #[test]
    fn experimental_mode_reentry_reuses_outer_lease_after_close() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();
        let (outer_entered_tx, outer_entered_rx) = std::sync::mpsc::sync_channel(0);
        let (finish_tx, finish_rx) = std::sync::mpsc::sync_channel(0);

        std::thread::scope(|scope| {
            let admission_ref = &admission;
            let callback = scope.spawn(move || unsafe {
                with_local_admission(admission_ref, |_| {
                    outer_entered_tx.send(()).unwrap();
                    finish_rx.recv().unwrap();

                    // Closing the process gate must not reject synchronous same-thread re-entry covered by the still-live outer token.
                    with_threaded_callback_admission_in(admission_ref, |_| {});
                });
            });

            outer_entered_rx.recv().unwrap();
            let close = scope.spawn(|| admission.close_and_wait());
            admission.wait_until_not_accepting();
            assert!(!close.is_finished());

            finish_tx.send(()).unwrap();
            callback.join().unwrap();
            close.join().unwrap();
        });
    }

    #[test]
    fn experimental_mode_unwind_releases_admission_lease() {
        let admission = ThreadedCallbackAdmission::default();
        admission.open();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            with_local_admission(&admission, |_| panic!("experimental admission unwind"));
        }));

        assert!(result.is_err());
        admission.close_and_wait();
    }
}

#[cfg(all(
    test,
    loom,
    not(feature = "experimental-threads"),
    not(feature = "codegen-lazy-fptrs")
))]
mod loom_threaded_callback_admission_tests {
    use std::sync::atomic::Ordering;

    use loom::sync::atomic::{AtomicBool, AtomicUsize};
    use loom::sync::{Arc, mpsc};
    use loom::thread;

    use super::{
        ThreadedCallbackAdmission, ThreadedCallbackAdmissionPhase,
        begin_threaded_callback_teardown_in, with_threaded_callback_admission_in,
    };

    fn check_admission_model(worker_count: usize, preemption_bound: Option<usize>) {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = preemption_bound;
        model.check(move || {
            let admission = Arc::new(ThreadedCallbackAdmission::default());
            admission.open();
            let bodies_started = Arc::new(AtomicUsize::new(0));
            let bodies_completed = Arc::new(AtomicUsize::new(0));
            let close_returned = Arc::new(AtomicBool::new(false));

            let workers: Vec<_> = (0..worker_count)
                .map(|_| {
                    let admission = Arc::clone(&admission);
                    let bodies_started = Arc::clone(&bodies_started);
                    let bodies_completed = Arc::clone(&bodies_completed);
                    let close_returned = Arc::clone(&close_returned);
                    thread::spawn(move || {
                        if let Some(lease) = admission.try_enter() {
                            bodies_started.fetch_add(1, Ordering::SeqCst);
                            thread::yield_now();
                            assert!(!close_returned.load(Ordering::SeqCst));
                            bodies_completed.fetch_add(1, Ordering::SeqCst);
                            drop(lease);
                            true
                        } else {
                            false
                        }
                    })
                })
                .collect();

            let closer_admission = Arc::clone(&admission);
            let closer_started = Arc::clone(&bodies_started);
            let closer_completed = Arc::clone(&bodies_completed);
            let closer_returned = Arc::clone(&close_returned);
            let closer = thread::spawn(move || {
                closer_admission.close_and_wait();
                assert_eq!(
                    closer_completed.load(Ordering::SeqCst),
                    closer_started.load(Ordering::SeqCst)
                );
                closer_returned.store(true, Ordering::SeqCst);
            });

            let admitted_workers = workers
                .into_iter()
                .map(|worker| usize::from(worker.join().unwrap()))
                .sum::<usize>();
            closer.join().unwrap();

            assert_eq!(bodies_started.load(Ordering::SeqCst), admitted_workers);
            assert_eq!(bodies_completed.load(Ordering::SeqCst), admitted_workers);
            assert!(close_returned.load(Ordering::SeqCst));
            {
                let state = admission.lock_state();
                assert_eq!(state.phase, ThreadedCallbackAdmissionPhase::Closed);
                assert_eq!(state.active, 0);
            }

            assert!(admission.try_enter().is_none());
        });
    }

    #[test]
    fn loom_exhausts_one_callback_against_binding_close_and_drain() {
        check_admission_model(1, None);
    }

    #[test]
    fn loom_bounds_two_callbacks_against_binding_close_and_drain() {
        // The bound keeps the multi-callback count/drain state space tractable while still exercising schedules where either admission or
        // close wins first and where the closer waits for one or both active leases.
        check_admission_model(2, Some(2));
    }

    #[test]
    fn loom_exhausts_synchronous_reentry_after_close_before_outer_lease_drains() {
        loom::model(|| {
            let admission = Arc::new(ThreadedCallbackAdmission::default());
            admission.open();
            let nested_bodies = Arc::new(AtomicUsize::new(0));
            let (outer_entered_tx, outer_entered_rx) = mpsc::channel();

            let worker_admission = Arc::clone(&admission);
            let worker_nested_bodies = Arc::clone(&nested_bodies);
            let worker = thread::spawn(move || unsafe {
                with_threaded_callback_admission_in(&worker_admission, |_| {
                    outer_entered_tx.send(()).unwrap();
                    worker_admission.wait_until_not_accepting();

                    // The outer synchronous stack still owns its lease. Re-entry after close must reuse it instead of trying to open a new
                    // admission, otherwise teardown can deadlock while waiting for the outer callback which is waiting for its nested call.
                    with_threaded_callback_admission_in(&worker_admission, |_| {
                        worker_nested_bodies.fetch_add(1, Ordering::SeqCst);
                    });
                });
            });

            outer_entered_rx.recv().unwrap();
            admission.close_and_wait();
            worker.join().unwrap();

            assert_eq!(nested_bodies.load(Ordering::SeqCst), 1);
            assert!(admission.try_enter().is_none());
        });
    }

    #[test]
    fn loom_worker_boundary_drains_workers_then_preserves_main_until_final_close() {
        loom::model(|| {
            let admission = Arc::new(ThreadedCallbackAdmission::default());
            admission.open();
            let worker_body_completed = Arc::new(AtomicBool::new(false));

            let worker_admission = Arc::clone(&admission);
            let worker_completed = Arc::clone(&worker_body_completed);
            let worker = thread::spawn(move || {
                if let Some(lease) = worker_admission.try_enter() {
                    thread::yield_now();
                    worker_completed.store(true, Ordering::SeqCst);
                    drop(lease);
                    true
                } else {
                    false
                }
            });

            begin_threaded_callback_teardown_in(&admission);
            let worker_was_admitted = worker.join().unwrap();
            assert_eq!(
                worker_body_completed.load(Ordering::SeqCst),
                worker_was_admitted
            );

            // The boundary caller is the recorded main thread and remains admissible while user/class teardown runs.
            assert!(admission.try_enter().is_some());

            let late_worker_admission = Arc::clone(&admission);
            let late_worker = thread::spawn(move || late_worker_admission.try_enter().is_none());
            assert!(late_worker.join().unwrap());

            admission.close_all_and_wait();
            admission.assert_closed_and_drained();
        });
    }
}

#[cfg(all(test, feature = "codegen-lazy-fptrs"))]
mod lazy_threaded_callback_tests {
    use super::with_threaded_callback_admission;

    #[test]
    #[should_panic(
        expected = "threaded callback scopes require eagerly initialized function tables"
    )]
    fn threaded_callback_scope_rejects_lazy_tables() {
        unsafe { with_threaded_callback_admission(|_| ()) };
    }
}

// Note, this is `Sync` and `Send` when "experimental-threads" is enabled because all its fields are. We have avoided implementing `Sync`
// and `Send` for `GodotBinding` as that could hide issues if any of the field types are changed to no longer be sync/send, but the manual
// implementation for `GodotBinding` wouldn't detect that.
pub(crate) struct GodotBinding {
    interface: GDExtensionInterface,
    get_proc_address: crate::GDExtensionInterfaceGetProcAddress,
    library: ClassLibraryPtr,
    global_method_table: BuiltinLifecycleTable,
    class_core_method_table: ManualInitCell<ClassCoreMethodTable>,
    class_servers_method_table: ManualInitCell<ClassServersMethodTable>,
    class_scene_method_table: ManualInitCell<ClassSceneMethodTable>,
    class_editor_method_table: ManualInitCell<ClassEditorMethodTable>,
    builtin_method_table: ManualInitCell<BuiltinMethodTable>,
    utility_function_table: UtilityFunctionTable,
    runtime_metadata: GdextRuntimeMetadata,
    config: GdextConfig,
    thread_safe_lifecycle: ThreadSafeLifecycle,
}

#[cfg(not(feature = "codegen-lazy-fptrs"))]
const _EAGER_GODOT_BINDING_IS_SEND_SYNC: () = crate::require_send_sync::<GodotBinding>();

// The reviewed thread-safe subset of `BuiltinLifecycleTable`. Field names must match the table 1:1; listed once here, so the struct
// declaration and `from_table` copy cannot drift. Each entry being a separate field is the guardrail: macros can only reach reviewed functions.
macro_rules! thread_safe_lifecycle {
    ($( $field:ident: $sig:ty ),* $(,)?) => {
        #[derive(Copy, Clone)]
        pub struct ThreadSafeLifecycle {
            $( pub $field: $sig, )*
        }

        impl ThreadSafeLifecycle {
            /// Picks the reviewed thread-safe subset out of the full lifecycle table. Built once at binding init, handed out by reference.
            fn from_table(table: &BuiltinLifecycleTable) -> Self {
                Self { $( $field: table.$field, )* }
            }
        }
    };
}

thread_safe_lifecycle! {
    string_construct_default: unsafe extern "C" fn(GDExtensionUninitializedTypePtr, *const GDExtensionConstTypePtr),
    string_construct_copy: unsafe extern "C" fn(GDExtensionUninitializedTypePtr, *const GDExtensionConstTypePtr),
    string_destroy: unsafe extern "C" fn(GDExtensionTypePtr),
    string_operator_equal: unsafe extern "C" fn(GDExtensionConstTypePtr, GDExtensionConstTypePtr, GDExtensionTypePtr),
    string_operator_less: unsafe extern "C" fn(GDExtensionConstTypePtr, GDExtensionConstTypePtr, GDExtensionTypePtr),
    string_from_string_name: unsafe extern "C" fn(GDExtensionUninitializedTypePtr, *const GDExtensionConstTypePtr),
    string_from_node_path: unsafe extern "C" fn(GDExtensionUninitializedTypePtr, *const GDExtensionConstTypePtr),
    string_to_variant: unsafe extern "C" fn(GDExtensionUninitializedVariantPtr, GDExtensionTypePtr),
    string_from_variant: unsafe extern "C" fn(GDExtensionUninitializedTypePtr, GDExtensionVariantPtr),
    string_name_construct_default: unsafe extern "C" fn(GDExtensionUninitializedTypePtr, *const GDExtensionConstTypePtr),
    string_name_construct_copy: unsafe extern "C" fn(GDExtensionUninitializedTypePtr, *const GDExtensionConstTypePtr),
    string_name_destroy: unsafe extern "C" fn(GDExtensionTypePtr),
    string_name_operator_equal: unsafe extern "C" fn(GDExtensionConstTypePtr, GDExtensionConstTypePtr, GDExtensionTypePtr),
    string_name_operator_less: unsafe extern "C" fn(GDExtensionConstTypePtr, GDExtensionConstTypePtr, GDExtensionTypePtr),
    string_name_from_string: unsafe extern "C" fn(GDExtensionUninitializedTypePtr, *const GDExtensionConstTypePtr),
    string_name_to_variant: unsafe extern "C" fn(GDExtensionUninitializedVariantPtr, GDExtensionTypePtr),
    string_name_from_variant: unsafe extern "C" fn(GDExtensionUninitializedTypePtr, GDExtensionVariantPtr),
}

impl GodotBinding {
    pub fn new(
        interface: GDExtensionInterface,
        get_proc_address: crate::GDExtensionInterfaceGetProcAddress,
        library: GDExtensionClassLibraryPtr,
        global_method_table: BuiltinLifecycleTable,
        utility_function_table: UtilityFunctionTable,
        runtime_metadata: GdextRuntimeMetadata,
        config: GdextConfig,
    ) -> Self {
        let thread_safe_lifecycle = ThreadSafeLifecycle::from_table(&global_method_table);

        Self {
            interface,
            get_proc_address,
            library: ClassLibraryPtr(library),
            global_method_table,
            thread_safe_lifecycle,
            class_core_method_table: ManualInitCell::new(),
            class_servers_method_table: ManualInitCell::new(),
            class_scene_method_table: ManualInitCell::new(),
            class_editor_method_table: ManualInitCell::new(),
            builtin_method_table: ManualInitCell::new(),
            utility_function_table,
            runtime_metadata,
            config,
        }
    }
}

/// Newtype around `GDExtensionClassLibraryPtr` so we can implement `Sync` and `Send` manually for this.
struct ClassLibraryPtr(crate::GDExtensionClassLibraryPtr);

// SAFETY: This implementation of `Sync` and `Send` does not guarantee that reading from or writing to the pointer is actually
// thread safe. It merely means we can send/share the pointer itself between threads. Which is safe since any place that actually
// reads/writes to this pointer must ensure they do so in a thread safe manner.
//
// So these implementations effectively just pass the responsibility for thread safe usage of the library pointer onto whomever
// reads/writes to the pointer from a different thread. Since doing so requires `unsafe` anyway this is something we can do soundly.
unsafe impl Sync for ClassLibraryPtr {}
// SAFETY: See `Sync` impl safety doc.
unsafe impl Send for ClassLibraryPtr {}

// ----------------------------------------------------------------------------------------------------------------------------------------------

/// # Safety
/// The table must not have been initialized yet.
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
unsafe fn initialize_table<T>(table: &ManualInitCell<T>, value: T, _what: &str) {
    crate::strict_assert!(
        !table.is_initialized(),
        "method table for {_what} should only be initialized once"
    );

    // SAFETY: One-time, non-shared access during init.
    table.set(value)
}

/// # Safety
/// The table must have been initialized.
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
unsafe fn get_table<T>(table: &'static ManualInitCell<T>, _msg: &str) -> &'static T {
    crate::strict_assert!(table.is_initialized(), "{_msg}");

    table.get_unchecked()
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Public API

/// # Safety
///
/// The Godot binding must have been initialized before calling this function.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn get_interface() -> &'static GDExtensionInterface {
    &get_binding().interface
}

/// Interface for FFI functions that touch shared engine/scene state and must run on the main thread.
///
/// Asserts main thread (non-disengaged profiles) plus binding-live check. For `classdb_*`, `object_method_bind_*`, etc.
/// Not a thread-safe alternative; use [`thread_safe()`] for functions that only touch caller-owned memory.
///
/// # Safety
/// The Godot binding must have been initialized before calling this function.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn on_main() -> &'static GDExtensionInterface {
    &get_binding().interface
}

/// Access the interface for thread-safe FFI functions (they only touch caller-owned memory).
///
/// Skips the main-thread assertion, keeping only the binding-live check. Default to [`on_main`] until a function is reviewed thread-safe.
/// Returns the full interface, so the restriction holds by convention only. TODO(v0.6): codegen a typed subset like [`thread_safe_lifecycle`],
/// so misuse becomes a compile error.
///
/// # Safety
/// The Godot binding must have been initialized before calling this function, and the accessed function must not touch shared engine state.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn thread_safe() -> &'static GDExtensionInterface {
    &get_binding_thread_safe().interface
}

/// # Safety
///
/// The Godot binding must have been initialized before calling this function.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn get_library() -> crate::GDExtensionClassLibraryPtr {
    get_binding().library.0
}

/// Looks up an FFI function by name using `get_proc_address`.
///
/// Argument must be null-terminated, e.g. `b"my_ffi_function\0"`.
///
/// Returns `None` if the function is not available (e.g. the runtime Godot version predates the function). Necessary only in niche cases
/// where polyfill/cross-version behavior needs to be emulated. Likely obsolete once there's `gdextension_interface.json`.
///
/// # Safety
/// The Godot binding must have been initialized before calling this function, and the function must be run on the main thread.
#[inline]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn get_ffi_ptr_by_cstr(name: &[u8]) -> crate::GDExtensionInterfaceFunctionPtr {
    let get_proc_address = get_binding()
        .get_proc_address
        .expect("get_proc_address should be available");

    get_proc_address(crate::c_str(name))
}

/// Access the builtin lifecycle table.
///
/// # Safety
///
/// The Godot binding must have been initialized before calling this function.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn builtin_lifecycle_api() -> &'static BuiltinLifecycleTable {
    &get_binding().global_method_table
}

/// Access the reviewed builtin lifecycle functions that are thread-safe.
///
/// Intentionally a narrow subset, not the full lifecycle table; additions must be reviewed individually.
///
/// # Safety
/// The Godot binding must have been initialized before calling this function, and the accessed function must not touch shared engine state.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn thread_safe_lifecycle() -> &'static ThreadSafeLifecycle {
    &get_binding_thread_safe().thread_safe_lifecycle
}

/// # Safety
///
/// - The Godot binding must have been initialized before calling this function.
/// - The class servers method table must have been initialized before calling this function.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn class_servers_api() -> &'static ClassServersMethodTable {
    get_table(
        &get_binding().class_servers_method_table,
        "cannot fetch classes; init level 'Servers' not yet loaded",
    )
}

/// # Safety
///
/// - The Godot binding must have been initialized before calling this function.
/// - The class core method table must have been initialized before calling this function.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn class_core_api() -> &'static ClassCoreMethodTable {
    get_table(
        &get_binding().class_core_method_table,
        "cannot fetch classes; init level 'Core' not yet loaded",
    )
}

/// # Safety
///
/// - The Godot binding must have been initialized before calling this function.
/// - The class scene method table must have been initialized before calling this function.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn class_scene_api() -> &'static ClassSceneMethodTable {
    get_table(
        &get_binding().class_scene_method_table,
        "cannot fetch classes; init level 'Scene' not yet loaded",
    )
}

/// # Safety
///
/// - The Godot binding must have been initialized before calling this function.
/// - The class editor method table must have been initialized before calling this function.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn class_editor_api() -> &'static ClassEditorMethodTable {
    get_table(
        &get_binding().class_editor_method_table,
        "cannot fetch classes; init level 'Editor' not yet loaded",
    )
}

/// # Safety
///
/// - The Godot binding must have been initialized before calling this function.
/// - The builtin method table must have been initialized before calling this function.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn builtin_method_table() -> &'static BuiltinMethodTable {
    get_table(
        &get_binding().builtin_method_table,
        "cannot fetch builtin methods; table not ready",
    )
}

/// # Safety
///
/// The Godot binding must have been initialized before calling this function.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn utility_function_table() -> &'static UtilityFunctionTable {
    &get_binding().utility_function_table
}

/// Access the utility-function table for functions reviewed as thread-safe (currently the print group and `str`).
///
/// Skips the main-thread assertion, keeping only the binding-live check. Most utility functions touch shared engine state and must therefore
/// keep going through [`utility_function_table`]; codegen routes only the individually reviewed ones here (see `is_utility_function_thread_safe`
/// in `godot-codegen`).
///
/// # Safety
///
/// The Godot binding must have been initialized before calling this function, and the accessed function must not touch shared engine state.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn utility_function_table_thread_safe() -> &'static UtilityFunctionTable {
    &get_binding_thread_safe().utility_function_table
}

/// # Safety
///
/// The Godot binding must have been initialized before calling this function.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[inline]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub unsafe fn config() -> &'static GdextConfig {
    &get_binding().config
}

/// Returns true if godot-rust bindings are initialized at a very low level.
///
/// The bindings are initialized when the library is loaded and deinitialized when it is unloaded (which may happen e.g. during a hot reload).
/// Do not use this as a general check whether certain Godot APIs can be used -- this is more complex and may depend on class/singleton.
#[inline]
pub fn is_godot_initialized() -> bool {
    BindingStorage::is_initialized()
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Crate-local implementation

/// Initializes the Godot binding.
///
/// Most other functions in this module rely on this function being called first as a safety condition.
///
/// # Safety
///
/// Must not be called concurrently with other functions that interact with the bindings - this is trivially true if "experimental-threads"
/// is not enabled.
///
/// If "experimental-threads" is enabled, then must be called from the main thread.
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub(crate) unsafe fn initialize_binding(binding: GodotBinding) {
    BindingStorage::initialize(binding);
}

/// Deinitializes the Godot binding.
///
/// # Safety
///
/// See [`initialize_binding`].
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub(crate) unsafe fn deinitialize_binding() {
    BindingStorage::deinitialize();
}

/// # Safety
///
/// The Godot binding must have been initialized before calling this function.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub(crate) unsafe fn get_binding() -> &'static GodotBinding {
    // Restricted access: assert main thread (no-op in multi-threaded builds), then perform the binding-live check.
    BindingStorage::ensure_main_thread();
    BindingStorage::get_binding_unchecked()
}

/// Like [`get_binding`], but without the main-thread assertion -- for thread-safe FFI functions.
///
/// # Safety
/// The Godot binding must have been initialized before calling this function. The accessed FFI function must only touch caller-owned memory
/// (e.g. builtin value-type ctors/dtors, mem alloc/free, type constructors), not shared engine or scene state.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub(crate) unsafe fn get_binding_thread_safe() -> &'static GodotBinding {
    BindingStorage::get_binding_unchecked()
}

/// # Safety
///
/// - The Godot binding must have been initialized before calling this function.
/// - Must only be called once.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub(crate) unsafe fn initialize_class_core_method_table(table: ClassCoreMethodTable) {
    initialize_table(
        &get_binding().class_core_method_table,
        table,
        "classes (Core level)",
    )
}

/// # Safety
///
/// - The Godot binding must have been initialized before calling this function.
/// - Must only be called once.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub(crate) unsafe fn initialize_class_servers_method_table(table: ClassServersMethodTable) {
    initialize_table(
        &get_binding().class_servers_method_table,
        table,
        "classes (Server level)",
    )
}

/// # Safety
///
/// - The Godot binding must have been initialized before calling this function.
/// - Must only be called once.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub(crate) unsafe fn initialize_class_scene_method_table(table: ClassSceneMethodTable) {
    initialize_table(
        &get_binding().class_scene_method_table,
        table,
        "classes (Scene level)",
    )
}

/// # Safety
///
/// The Godot binding must have been initialized before calling this function.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[inline(always)]
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub(crate) unsafe fn runtime_metadata() -> &'static GdextRuntimeMetadata {
    &get_binding().runtime_metadata
}

/// # Safety
///
/// - The Godot binding must have been initialized before calling this function.
/// - Must only be called once.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub(crate) unsafe fn initialize_class_editor_method_table(table: ClassEditorMethodTable) {
    initialize_table(
        &get_binding().class_editor_method_table,
        table,
        "classes (Editor level)",
    )
}

/// # Safety
///
/// - The Godot binding must have been initialized before calling this function.
/// - Must only be called once.
///
/// If "experimental-threads" is not enabled, then this must be called from the same thread that the bindings were initialized from.
#[allow(unsafe_op_in_unsafe_fn)] // Safety preconditions forwarded 1:1.
pub(crate) unsafe fn initialize_builtin_method_table(table: BuiltinMethodTable) {
    initialize_table(&get_binding().builtin_method_table, table, "builtins")
}
