/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use godot::classes::{IResourceFormatLoader, Resource, ResourceFormatLoader};
use godot::prelude::*;

struct HotReload;

#[gdextension]
unsafe impl ExtensionLibrary for HotReload {
    fn on_stage_init(stage: InitStage) {
        println!("[Rust]      Init stage {stage:?}");
    }

    fn on_stage_deinit(stage: InitStage) {
        println!("[Rust]      Deinit stage {stage:?}");
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------

#[derive(GodotClass)]
#[class(init, base=Node)]
struct Reloadable {
    #[export]
    #[init(val = Planet::Earth)]
    favorite_planet: Planet,

    #[init(val = NoDefault::obtain())]
    _other_object: Gd<NoDefault>,
}

#[godot_api]
impl Reloadable {
    #[func]
    #[rustfmt::skip]
    // DO NOT MODIFY FOLLOWING LINE -- replaced by hot-reload test. Hence #[rustfmt::skip] above.
    fn get_number(&self) -> i64 { 100 }

    #[func]
    fn from_string(s: GString) -> Gd<Self> {
        Gd::from_object(Reloadable {
            favorite_planet: Planet::from_godot(s),
            _other_object: NoDefault::obtain(),
        })
    }
}

// no_init reloadability - https://github.com/godot-rust/gdext/issues/874.
#[derive(GodotClass)]
#[class(no_init, base=Node)]
struct NoDefault {}

#[godot_api]
impl NoDefault {
    #[func]
    fn obtain() -> Gd<Self> {
        Gd::from_object(NoDefault {})
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------

/// Exercises hot-reload replacement for the opaque-handle storage path. No virtual callback is
/// selected for worker dispatch; the opt-in is used here solely so this instance is registered with
/// blocking storage before and after the extension reload.
#[derive(GodotClass)]
#[class(init, base=Node, unsafe_threaded)]
struct ThreadedReloadable {
    #[export]
    #[init(val = 313)]
    marker: i64,
}

// SAFETY: this test class has no worker-selected virtuals, contains only `Send` Rust data, and its
// fields perform no Godot calls from `Drop`.
unsafe impl godot::obj::ThreadedGodotClass for ThreadedReloadable {}

#[godot_api]
impl ThreadedReloadable {
    #[func]
    fn doubled_marker(&self) -> i64 {
        self.marker * 2
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------

const THREADED_RELOAD_EXTENSION: &str = "gdext_threaded_hot_reload";
const THREADED_RELOAD_WAIT_TIMEOUT: Duration = Duration::from_secs(15);

static THREADED_RELOAD_MAIN_THREAD: Mutex<Option<usize>> = Mutex::new(None);
static THREADED_RELOAD_RELEASE_PATH: Mutex<String> = Mutex::new(String::new());
static THREADED_RELOAD_COMPLETED_PATH: Mutex<String> = Mutex::new(String::new());
static THREADED_RELOAD_TIMEOUT_PATH: Mutex<String> = Mutex::new(String::new());
static THREADED_RELOAD_ARMED: AtomicBool = AtomicBool::new(false);
static THREADED_RELOAD_ENTERED: AtomicBool = AtomicBool::new(false);
static THREADED_RELOAD_LAST_CALLBACK_WAS_WORKER: AtomicBool = AtomicBool::new(false);
static THREADED_RELOAD_LOAD_COUNT: AtomicUsize = AtomicUsize::new(0);

fn current_thread_id() -> usize {
    thread_local! {
        static CURRENT_THREAD_MARKER: u8 = const { 0 };
    }

    CURRENT_THREAD_MARKER.with(|marker| marker as *const u8 as usize)
}

#[derive(GodotClass)]
#[class(no_init, base=Object)]
struct ThreadedReloadProbe;

#[godot_api]
impl ThreadedReloadProbe {
    #[func]
    fn arm_gate(release_path: GString, completed_path: GString, timeout_path: GString) {
        *THREADED_RELOAD_MAIN_THREAD.lock().unwrap() = Some(current_thread_id());
        *THREADED_RELOAD_RELEASE_PATH.lock().unwrap() = release_path.to_string();
        *THREADED_RELOAD_COMPLETED_PATH.lock().unwrap() = completed_path.to_string();
        *THREADED_RELOAD_TIMEOUT_PATH.lock().unwrap() = timeout_path.to_string();
        THREADED_RELOAD_ENTERED.store(false, Ordering::SeqCst);
        THREADED_RELOAD_LAST_CALLBACK_WAS_WORKER.store(false, Ordering::SeqCst);
        THREADED_RELOAD_LOAD_COUNT.store(0, Ordering::SeqCst);
        THREADED_RELOAD_ARMED.store(true, Ordering::SeqCst);
    }

    #[func]
    fn arm_open_probe() {
        *THREADED_RELOAD_MAIN_THREAD.lock().unwrap() = Some(current_thread_id());
        THREADED_RELOAD_ENTERED.store(false, Ordering::SeqCst);
        THREADED_RELOAD_LAST_CALLBACK_WAS_WORKER.store(false, Ordering::SeqCst);
        THREADED_RELOAD_LOAD_COUNT.store(0, Ordering::SeqCst);
        THREADED_RELOAD_ARMED.store(false, Ordering::SeqCst);
    }

    #[func]
    fn callback_entered() -> bool {
        THREADED_RELOAD_ENTERED.load(Ordering::SeqCst)
    }

    #[func]
    fn last_callback_was_worker() -> bool {
        THREADED_RELOAD_LAST_CALLBACK_WAS_WORKER.load(Ordering::SeqCst)
    }

    #[func]
    fn load_count() -> i64 {
        THREADED_RELOAD_LOAD_COUNT.load(Ordering::SeqCst) as i64
    }
}

/// A real engine-dispatched worker callback kept alive across the instant hot reload starts.
///
/// The callback gate is released by the external hot-reload harness through a marker file. This is
/// deliberate: calling another extension method from a helper thread during unload would itself be
/// a callback-admission race, while a detached Rust helper could outlive the dylib that contains
/// its code.
#[derive(GodotClass)]
#[class(init, tool, base=ResourceFormatLoader, unsafe_threaded)]
struct ThreadedHotReloadLoader {
    base: Base<ResourceFormatLoader>,
}

// SAFETY: selected virtuals use synchronized Rust globals plus `std::fs`, and never access `base`
// or another Godot object. The owner keeps this loader registered and strongly referenced through
// callback drain, hot reload, and the engine's subsequent request publication/cleanup, then
// unregisters it before releasing the final reference. No field calls Godot from `Drop`.
unsafe impl godot::obj::ThreadedGodotClass for ThreadedHotReloadLoader {}

#[godot_api(unsafe_threaded)]
impl IResourceFormatLoader for ThreadedHotReloadLoader {
    fn get_recognized_extensions(&self) -> PackedStringArray {
        [GString::from(THREADED_RELOAD_EXTENSION)]
            .into_iter()
            .collect()
    }

    fn get_resource_type(&self, _path: GString) -> GString {
        "ThreadedHotReloadResource".into()
    }

    fn load(
        &self,
        _path: GString,
        _original_path: GString,
        _use_sub_threads: bool,
        _cache_mode: i32,
    ) -> Variant {
        THREADED_RELOAD_LOAD_COUNT.fetch_add(1, Ordering::SeqCst);

        let main_thread = *THREADED_RELOAD_MAIN_THREAD.lock().unwrap();
        THREADED_RELOAD_LAST_CALLBACK_WAS_WORKER.store(
            main_thread.is_some_and(|main| main != current_thread_id()),
            Ordering::SeqCst,
        );

        if THREADED_RELOAD_ARMED.swap(false, Ordering::SeqCst) {
            THREADED_RELOAD_ENTERED.store(true, Ordering::SeqCst);

            let release_path = THREADED_RELOAD_RELEASE_PATH.lock().unwrap().clone();
            let deadline = Instant::now() + THREADED_RELOAD_WAIT_TIMEOUT;
            while !std::path::Path::new(&release_path).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }

            if std::path::Path::new(&release_path).exists() {
                let completed_path = THREADED_RELOAD_COMPLETED_PATH.lock().unwrap().clone();
                std::fs::write(
                    completed_path,
                    b"callback completed after external release\n",
                )
                .expect("write threaded hot-reload completion marker");
            } else {
                let timeout_path = THREADED_RELOAD_TIMEOUT_PATH.lock().unwrap().clone();
                let _ = std::fs::write(timeout_path, b"threaded hot-reload callback timed out\n");
            }
        }

        Resource::new_gd().to_variant()
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------

#[derive(Clone, GodotConvert, Var, Export)]
#[godot(via = GString)]
enum Planet {
    Earth,
    Mars,
    Venus,
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Signal soundness: https://github.com/godot-rust/gdext/pull/1512.

#[cfg(feature = "signal-test")]
pub use signal_test::*;

#[cfg(feature = "signal-test")]
mod signal_test {
    use godot::classes::notify::ObjectNotification;

    use super::*;

    #[derive(GodotClass)]
    #[class(init, tool, base = Object)]
    struct Signaller {
        #[var]
        value: i64,
        base: Base<Object>,
    }

    #[godot_api]
    impl IObject for Signaller {
        fn on_notification(&mut self, what: ObjectNotification) {
            // Recreate signal after hot reload.
            // Doesn't really matter much for this test - it does NOT replace previous signal (which should be disconnected at this point).
            if what == ObjectNotification::EXTENSION_RELOADED {
                self.signals()
                    .reloadable_signal()
                    .connect_self(|this, val| this.value = val);
            }
        }
    }

    #[godot_api]
    impl Signaller {
        // Regression test for https://github.com/godot-rust/gdext/issues/984: a Rust callable connected via the untyped `Object::connect`
        // API must be auto-disconnected before hot reload, else the stale callable is touched when the object is freed afterwards and
        // segfaults. `is_rust_callable()` detects it via our extension token, so it is registered and disconnected. No crash = pass.
        #[func]
        fn connect_custom_callables(&mut self) {
            let plain = Callable::from_fn("rust_fn", |_| Variant::nil());
            self.base_mut().connect("property_list_changed", &plain);
        }

        // Characterization test for the false positive in https://github.com/godot-rust/gdext/issues/984: a bound *engine* method is
        // `is_custom()` but not `is_rust_callable()`, so the narrowed registry does not register it -- correctly leaving it connected, as it
        // resolves by name and is safe across reload. `ReloadTest.gd` asserts it survives. If the registry is later broadened, that flips.
        #[func]
        fn connect_bound_engine_method(&mut self) {
            let receiver = self.to_gd();
            let bound =
                Callable::from_object_method(&receiver, "set_block_signals").bind(vslice![false]);
            self.base_mut().connect("script_changed", &bound);
        }

        #[func]
        fn initialize_connections(&mut self) {
            self.signals()
                .reloadable_signal()
                .connect_self(|this, val| this.value = val);

            // Given RefCounted will be instantly freed after this function execution,
            // but signal connection will be registered and pruned upon hot reload.
            let some_refcounted = RefCounted::new_gd();
            some_refcounted
                .signals()
                .property_list_changed()
                .connect(|| godot_print!("Henlo!"));
        }

        #[signal]
        fn reloadable_signal(val: i64);
    }
}
