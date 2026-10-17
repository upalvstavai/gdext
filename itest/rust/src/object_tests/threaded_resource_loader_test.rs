/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use godot::classes::resource_loader::{CacheMode, ThreadLoadStatus};
use godot::classes::{IResourceFormatLoader, Resource, ResourceFormatLoader, ResourceLoader};
use godot::global::Error;
use godot::obj::{EngineEnum, Singleton};
use godot::prelude::*;

use crate::framework::itest;

const EXTENSION: &str = "gdext_threaded_loader_itest";
const RESOURCE_TYPE: &str = "GDExtThreadedLoaderITestResource";
const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const LIFETIME_PATH: &str = "res://gdext_threaded_loader_lifetime.gdext_threaded_loader_itest";

#[derive(Clone, Copy)]
struct CallbackObservation {
    method: &'static str,
    thread: ThreadId,
}

struct ThreadedLoaderState {
    main_thread: ThreadId,
    observations: Mutex<Vec<CallbackObservation>>,
    loaded_paths: Mutex<Vec<String>>,
    load_count: AtomicUsize,
    active_loads: AtomicUsize,
    max_active_loads: AtomicUsize,
    sub_thread_load_count: AtomicUsize,
    lifetime_callback_entered: AtomicBool,
    lifetime_callback_release: Mutex<bool>,
    lifetime_callback_wakeup: Condvar,
    lifetime_callback_timed_out: AtomicBool,
}

impl ThreadedLoaderState {
    fn new(main_thread: ThreadId) -> Self {
        Self {
            main_thread,
            observations: Mutex::new(Vec::new()),
            loaded_paths: Mutex::new(Vec::new()),
            load_count: AtomicUsize::new(0),
            active_loads: AtomicUsize::new(0),
            max_active_loads: AtomicUsize::new(0),
            sub_thread_load_count: AtomicUsize::new(0),
            lifetime_callback_entered: AtomicBool::new(false),
            lifetime_callback_release: Mutex::new(false),
            lifetime_callback_wakeup: Condvar::new(),
            lifetime_callback_timed_out: AtomicBool::new(false),
        }
    }

    fn observe(&self, method: &'static str) {
        self.observations.lock().unwrap().push(CallbackObservation {
            method,
            thread: std::thread::current().id(),
        });
    }

    fn begin_load(&self, path: String, use_sub_threads: bool) {
        self.observe("load");
        self.loaded_paths.lock().unwrap().push(path);
        self.load_count.fetch_add(1, Ordering::SeqCst);
        if use_sub_threads {
            self.sub_thread_load_count.fetch_add(1, Ordering::SeqCst);
        }

        let active = self.active_loads.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active_loads.fetch_max(active, Ordering::SeqCst);
    }

    fn wait_in_lifetime_callback(&self) {
        self.lifetime_callback_entered.store(true, Ordering::SeqCst);

        let released = self.lifetime_callback_release.lock().unwrap();
        let (released, timeout) = self
            .lifetime_callback_wakeup
            .wait_timeout_while(released, WAIT_TIMEOUT, |released| !*released)
            .unwrap();

        if timeout.timed_out() && !*released {
            self.lifetime_callback_timed_out
                .store(true, Ordering::SeqCst);
        }
    }

    fn release_lifetime_callback(&self) {
        *self.lifetime_callback_release.lock().unwrap() = true;
        self.lifetime_callback_wakeup.notify_all();
    }
}

#[derive(GodotClass)]
#[class(no_init, base=ResourceFormatLoader, unsafe_threaded)]
struct ThreadedFormatLoaderTest {
    state: Arc<ThreadedLoaderState>,
    base: Base<ResourceFormatLoader>,
}

// SAFETY: every regular virtual in this interface implementation uses only synchronized Rust state
// and worker-safe value marshalling, never accesses `base` or a retained Godot object, and creates
// only the Resource that `load` must return. The test unregisters the loader and drains every
// request before returning; this class and its fields perform no Godot calls from `Drop`.
unsafe impl godot::obj::ThreadedGodotClass for ThreadedFormatLoaderTest {}

#[godot_api(unsafe_threaded)]
impl IResourceFormatLoader for ThreadedFormatLoaderTest {
    fn get_recognized_extensions(&self) -> PackedStringArray {
        self.state.observe("get_recognized_extensions");
        [GString::from(EXTENSION)].into_iter().collect()
    }

    fn get_resource_type(&self, _path: GString) -> GString {
        self.state.observe("get_resource_type");
        RESOURCE_TYPE.into()
    }

    fn load(
        &self,
        path: GString,
        _original_path: GString,
        use_sub_threads: bool,
        _cache_mode: i32,
    ) -> Variant {
        let path = path.to_string();
        self.state.begin_load(path.clone(), use_sub_threads);

        if path == LIFETIME_PATH {
            self.state.wait_in_lifetime_callback();
        } else {
            // Keep the first batch in flight long enough for multiple requests to be outstanding.
            std::thread::sleep(Duration::from_millis(5));
        }

        self.state.active_loads.fetch_sub(1, Ordering::SeqCst);
        Resource::new_gd().to_variant()
    }
}

struct LoaderRegistration {
    loader: Gd<ResourceLoader>,
    format_loader: Option<Gd<ThreadedFormatLoaderTest>>,
    state: Arc<ThreadedLoaderState>,
    registered: bool,
}

impl LoaderRegistration {
    fn new(
        mut loader: Gd<ResourceLoader>,
        format_loader: Gd<ThreadedFormatLoaderTest>,
        state: Arc<ThreadedLoaderState>,
    ) -> Self {
        loader
            .add_resource_format_loader_ex(&format_loader)
            .at_front(true)
            .done();
        Self {
            loader,
            format_loader: Some(format_loader),
            state,
            registered: true,
        }
    }

    fn unregister(&mut self) {
        let format_loader = self
            .format_loader
            .as_ref()
            .expect("format loader must remain alive while registered");
        self.loader.remove_resource_format_loader(format_loader);
        self.registered = false;
    }

    fn drop_format_loader(&mut self) {
        drop(self.format_loader.take());
    }
}

impl Drop for LoaderRegistration {
    fn drop(&mut self) {
        // A test panic must not strand the gated callback or leave the custom extension registered
        // for subsequent tests in the same Godot process.
        self.state.release_lifetime_callback();
        let deadline = Instant::now() + WAIT_TIMEOUT;
        while self.state.active_loads.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if self.registered {
            if let Some(format_loader) = self.format_loader.as_ref() {
                self.loader.remove_resource_format_loader(format_loader);
            }
            self.registered = false;
        }

        // A hung worker already means the test is failing. Leaking its final reference is safer than
        // destroying the object inside a Godot-to-Rust callback and turning that failure into UB.
        if self.state.active_loads.load(Ordering::SeqCst) != 0
            && let Some(format_loader) = self.format_loader.take()
        {
            std::mem::forget(format_loader);
        }
    }
}

fn wait_until(description: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

// The typed ResourceLoader thread APIs are deliberately generated only with the global
// `experimental-threads` feature. This test leaves that feature off so `#[class(unsafe_threaded)]`,
// rather than the global blocking-storage fallback, is what enables the callback. These dynamic
// calls stay on the main thread and invoke the same Godot methods without weakening that condition.
fn request_threaded_load(loader: &mut Gd<ResourceLoader>, path: &str) -> Error {
    let error = loader
        .call(
            "load_threaded_request",
            vslice![path, "", true, CacheMode::IGNORE.ord()],
        )
        .to::<i64>() as i32;
    Error::try_from_ord(error).unwrap_or_else(|| panic!("unknown Godot error code {error}"))
}

fn threaded_load_status(loader: &mut Gd<ResourceLoader>, path: &str) -> ThreadLoadStatus {
    let status = loader
        .call("load_threaded_get_status", vslice![path])
        .to::<i64>() as i32;
    ThreadLoadStatus::try_from_ord(status)
        .unwrap_or_else(|| panic!("unknown threaded load status {status}"))
}

fn wait_for_threaded_load(loader: &mut Gd<ResourceLoader>, path: &str) -> Gd<Resource> {
    wait_until(&format!("threaded resource load `{path}`"), || {
        threaded_load_status(loader, path) != ThreadLoadStatus::IN_PROGRESS
    });

    let status = threaded_load_status(loader, path);
    // Always collect the request, including on failure; Godot otherwise retains its load token.
    let resource = loader
        .call("load_threaded_get", vslice![path])
        .try_to::<Gd<Resource>>();
    assert_eq!(
        status,
        ThreadLoadStatus::LOADED,
        "threaded resource load `{path}` ended with {status:?}"
    );
    resource.unwrap_or_else(|error| {
        panic!("threaded resource load `{path}` returned no resource: {error}")
    })
}

#[itest]
fn threaded_format_loader_worker_callbacks() {
    const LOAD_COUNT: usize = 6;

    let state = Arc::new(ThreadedLoaderState::new(std::thread::current().id()));
    let format_loader = Gd::from_init_fn({
        let state = state.clone();
        move |base| ThreadedFormatLoaderTest { state, base }
    });

    let mut registration =
        LoaderRegistration::new(ResourceLoader::singleton(), format_loader, state.clone());

    let paths = (0..LOAD_COUNT)
        .map(|index| format!("res://gdext_threaded_loader_{index}.{EXTENSION}"))
        .collect::<Vec<_>>();

    for path in &paths {
        let error = request_threaded_load(&mut registration.loader, path);
        assert_eq!(error, Error::OK, "failed to request threaded load `{path}`");
    }
    for path in &paths {
        drop(wait_for_threaded_load(&mut registration.loader, path));
    }

    let error = request_threaded_load(&mut registration.loader, LIFETIME_PATH);
    assert_eq!(
        error,
        Error::OK,
        "failed to request the loader lifetime probe"
    );
    wait_until("the loader lifetime callback to start", || {
        state.lifetime_callback_entered.load(Ordering::SeqCst)
    });

    // Keep both Godot's registration and the caller's strong reference alive until the callback
    // and load token are drained. Godot 4.7 does not keep an independent lifetime-safe reference
    // after unregistering: dropping the final `Gd` during this callback produces its
    // engine-use-after-free diagnostic. Unregistering concurrently with loader selection is also
    // an engine-level synchronization question, so this test deliberately avoids relying on it.
    let retained_while_active = state.active_loads.load(Ordering::SeqCst) == 1;
    state.release_lifetime_callback();
    drop(wait_for_threaded_load(
        &mut registration.loader,
        LIFETIME_PATH,
    ));
    registration.unregister();
    registration.drop_format_loader();

    let lifetime_callback_timed_out = state.lifetime_callback_timed_out.load(Ordering::SeqCst);
    assert!(
        retained_while_active,
        "the owner did not retain the loader while its lifetime callback was active"
    );
    assert!(
        !lifetime_callback_timed_out,
        "the lifetime callback's anti-deadlock timeout fired before explicit release"
    );

    let expected_load_count = LOAD_COUNT + 1;
    assert_eq!(state.load_count.load(Ordering::SeqCst), expected_load_count);
    assert_eq!(
        state.sub_thread_load_count.load(Ordering::SeqCst),
        expected_load_count,
        "every callback should receive use_sub_threads=true"
    );
    assert_eq!(
        state.active_loads.load(Ordering::SeqCst),
        0,
        "all worker callbacks must finish before test cleanup"
    );
    assert_eq!(
        state.max_active_loads.load(Ordering::SeqCst),
        1,
        "blocking instance storage must serialize callbacks to one loader instance"
    );

    let seen_paths = state
        .loaded_paths
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let expected_paths = paths
        .iter()
        .cloned()
        .chain([LIFETIME_PATH.to_owned()])
        .collect::<HashSet<_>>();
    assert_eq!(seen_paths, expected_paths);

    let observations = state.observations.lock().unwrap();
    let worker_methods = observations
        .iter()
        .filter(|observation| observation.thread != state.main_thread)
        .map(|observation| observation.method)
        .collect::<HashSet<_>>();
    assert!(
        worker_methods.contains("get_recognized_extensions"),
        "Godot must select the loader through a worker-thread callback"
    );
    assert!(
        worker_methods.contains("load"),
        "Godot must invoke ResourceFormatLoader::load off the main thread"
    );
    assert!(
        observations
            .iter()
            .filter(|observation| observation.method == "load")
            .all(|observation| observation.thread != state.main_thread),
        "every ResourceFormatLoader::load callback must run off the test's main thread"
    );
}
