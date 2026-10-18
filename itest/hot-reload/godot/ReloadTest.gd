# Copyright (c) godot-rust; Bromeon and contributors.
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Note: This file is not part of the example, but a script used for gdext integration tests.
# You can safely ignore it.

@tool
extends Node

var udp := PacketPeerUDP.new()
var thread := Thread.new()
var extension_name: String

var retained_obj: Reloadable
var threaded_retained_obj: ThreadedReloadable
var threaded_loader_obj: ThreadedHotReloadLoader

var signal_obj: Object

const THREADED_PRE_PATH := "res://before_reload.gdext_threaded_hot_reload"
const THREADED_POST_PATH := "res://after_reload.gdext_threaded_hot_reload"
const THREADED_RELEASE_FILE := "res://.threaded_reload_release"
const THREADED_STARTED_FILE := "res://.threaded_reload_started"
const THREADED_COMPLETED_FILE := "res://.threaded_reload_completed"
const THREADED_TIMEOUT_FILE := "res://.threaded_reload_timeout"
const MIN_RELOAD_WAIT_MSEC := 250

var initial_ready_sent := false
var waiting_for_post_reload := false
var post_request_started := false
var threaded_loader_registered := false


func _ready() -> void:
	print("[GD Editor] Start...")

	var r = Reloadable.new()
	var num = r.get_number()
	r.free()

	# Test construction from Rust (regression test for https://github.com/godot-rust/gdext/issues/543).
	var rust_constructed = Reloadable.from_string("Mars")
	var rust_constructed_planet = rust_constructed.favorite_planet
	rust_constructed.free()
	if rust_constructed_planet != "Mars":
		fail(str("Rust-constructed Reloadable did not retain its property (is ", rust_constructed_planet, ")"))
		return

	# Keep a ClassDB-created object alive across the reload so Godot tracks it on every supported version.
	retained_obj = Reloadable.new()
	retained_obj.favorite_planet = "Mars"
	var planet = retained_obj.favorite_planet
	threaded_retained_obj = ThreadedReloadable.new()
	threaded_retained_obj.marker = 451

	print("[GD Editor] Sanity check: initial number is ", num, "; planet is ", planet)

	var extensions = GDExtensionManager.get_loaded_extensions()
	if extensions.size() == 1:
		extension_name = extensions[0]
	else:
		fail(str("Must have 1 extension, has: ", extensions))
		return

	if ClassDB.class_exists("Signaller"):
		self.signal_obj = ClassDB.instantiate("Signaller")
		self.signal_obj.connect_custom_callables()
		self.signal_obj.connect_bound_engine_method()
		self.signal_obj.reloadable_signal.emit(42)
		print("[GD Editor] Sanity check: Signaller emitted signal and holds ", self.signal_obj.value)
	else:
		print("[GD Editor] Skipping signal test: hot reloading with typed signals is not supported for this Godot version")

	udp.bind(1337)

	threaded_loader_obj = ThreadedHotReloadLoader.new()
	ResourceLoader.add_resource_format_loader(threaded_loader_obj, true)
	threaded_loader_registered = true
	ThreadedReloadProbe.arm_gate(
		ProjectSettings.globalize_path(THREADED_RELEASE_FILE),
		ProjectSettings.globalize_path(THREADED_COMPLETED_FILE),
		ProjectSettings.globalize_path(THREADED_TIMEOUT_FILE),
	)
	var request_error := ResourceLoader.load_threaded_request(
		THREADED_PRE_PATH,
		"",
		true,
		ResourceLoader.CACHE_MODE_IGNORE,
	)
	if request_error != OK:
		fail(str("Failed to start pre-reload threaded callback: ", request_error))
		return

	print("[GD Editor] Waiting for selected worker callback before reload...")


func send_udp():
	var out_udp = PacketPeerUDP.new()

	if out_udp.set_dest_address("127.0.0.1", 1338) != OK:
		fail("Failed to set destination address")
		return

	if out_udp.put_packet("ready".to_utf8_buffer()) != OK:
		fail("Failed to send packet")
		return

	print("[GD Editor] Packet sent successfully")
	out_udp.close()


func _exit_tree() -> void:
	print("[GD Editor] ReloadTest exit.")
	if threaded_loader_registered and is_instance_valid(threaded_loader_obj):
		ResourceLoader.remove_resource_format_loader(threaded_loader_obj)
		threaded_loader_registered = false
	udp.close()


func _process(delta: float) -> void:
	if not initial_ready_sent:
		if not ThreadedReloadProbe.callback_entered():
			return
		if not ThreadedReloadProbe.last_callback_was_worker():
			fail("Selected ResourceFormatLoader callback did not run on a worker thread")
			return
		initial_ready_sent = true
		print("[GD Editor] Worker callback is gated; ReloadTest ready to receive...")
		send_udp()
		return

	if waiting_for_post_reload:
		_poll_post_reload_request()
		return

	if udp.get_available_packet_count() == 0:
		return

	var packet = udp.get_packet().get_string_from_ascii()
	print("[GD Editor] Received UDP packet [", packet.length(), "]: ", packet)

	var started_file := FileAccess.open(THREADED_STARTED_FILE, FileAccess.WRITE)
	if started_file == null:
		fail("Failed to write threaded reload start marker")
		return
	started_file.store_line("reload_extension call begins")
	started_file.close()

	var reload_started_msec := Time.get_ticks_msec()
	if not _hot_reload():
		return
	var reload_elapsed_msec := Time.get_ticks_msec() - reload_started_msec

	if reload_elapsed_msec < MIN_RELOAD_WAIT_MSEC:
		fail(str("Hot reload returned before the gated worker callback was released (", reload_elapsed_msec, " ms)"))
		return
	if not FileAccess.file_exists(THREADED_COMPLETED_FILE):
		fail("Hot reload completed without the worker callback completion marker")
		return
	if FileAccess.file_exists(THREADED_TIMEOUT_FILE):
		fail("Worker callback escaped only through its anti-deadlock timeout")
		return
	print("[GD Editor] Hot reload waited ", reload_elapsed_msec, " ms for worker callback drain")

	# Collect the pre-reload request first. Callback drain ends at the generated trampoline boundary,
	# while Godot may still need a frame to publish the completed threaded-load token.
	waiting_for_post_reload = true


func _poll_post_reload_request() -> void:
	if not post_request_started:
		var pre_status := ResourceLoader.load_threaded_get_status(THREADED_PRE_PATH)
		if pre_status == ResourceLoader.THREAD_LOAD_IN_PROGRESS:
			return
		if pre_status != ResourceLoader.THREAD_LOAD_LOADED:
			fail(str("Pre-reload threaded callback ended with status ", pre_status))
			return
		var pre_resource := ResourceLoader.load_threaded_get(THREADED_PRE_PATH)
		if pre_resource == null:
			fail("Pre-reload threaded callback returned no Resource")
			return

		# The library has now been reloaded. Exercise the same reconstructed loader again so this
		# proves that callback admission opened for the next lifecycle, not only that teardown drained.
		ThreadedReloadProbe.arm_open_probe()
		var post_request_error := ResourceLoader.load_threaded_request(
			THREADED_POST_PATH,
			"",
			true,
			ResourceLoader.CACHE_MODE_IGNORE,
		)
		if post_request_error != OK:
			fail(str("Failed to start post-reload threaded callback: ", post_request_error))
			return
		post_request_started = true
		return

	var status := ResourceLoader.load_threaded_get_status(THREADED_POST_PATH)
	if status == ResourceLoader.THREAD_LOAD_IN_PROGRESS:
		return
	if status != ResourceLoader.THREAD_LOAD_LOADED:
		fail(str("Post-reload threaded callback ended with status ", status))
		return

	var resource := ResourceLoader.load_threaded_get(THREADED_POST_PATH)
	if resource == null:
		fail("Post-reload threaded callback returned no Resource")
		return
	if ThreadedReloadProbe.load_count() < 1:
		fail("Reopened lifecycle did not invoke the threaded loader")
		return
	if not ThreadedReloadProbe.last_callback_was_worker():
		fail("Reopened lifecycle invoked the selected callback on the main thread")
		return

	waiting_for_post_reload = false
	ResourceLoader.remove_resource_format_loader(threaded_loader_obj)
	threaded_loader_registered = false
	threaded_loader_obj = null
	print("[GD Editor] Post-reload worker callback succeeded after lifecycle reopened")
	_finalize_hot_reload_assertions()


func _finalize_hot_reload_assertions() -> void:

	if self.signal_obj:
		# Test if previous signal has been properly disconnected before hot-reload.
		self.signal_obj.reloadable_signal.emit(12)
		# We did not crash thus all works well :).
		print("[GD Editor] Signal has been properly hot reloaded!")

		# The bound engine-method connection is safe across reload, and the registry only auto-disconnects callables created by this extension
		# (see `Callable::is_rust_callable()`). A bound engine method is not one (even if it binds a Rust callable), so the connection survives.
		if self.signal_obj.get_signal_connection_list("script_changed").is_empty():
			fail("Connection to bind()-Callable dropped on reload")
			return

		self.signal_obj.free()

	var r = Reloadable.new()
	var num = r.get_number()
	r.free()

	# Check if the property has been restored.
	var planet = retained_obj.favorite_planet
	retained_obj.free()
	var threaded_marker = threaded_retained_obj.marker
	var threaded_double = threaded_retained_obj.doubled_marker()
	threaded_retained_obj.free()

	if num == 777 and planet == "Mars" and threaded_marker == 451 and threaded_double == 902:
		print("[GD Editor] Successful hot-reload! Exit...")
		get_tree().quit(0)
	elif num != 777:
		fail(str("Number was not updated correctly (is ", num, ")"))
		return
	elif planet != "Mars":
		fail(str("Planet was not restored correctly (is ", planet, ")"))
		return
	else:
		fail(str("Threaded storage was not restored correctly (marker ", threaded_marker, ", doubled ", threaded_double, ")"))
		return


func _hot_reload():
	var status = GDExtensionManager.reload_extension(extension_name)
	if status != OK:
		fail(str("Failed to reload extension: ", status))
		return false

	return true


func fail(s: String) -> void:
	print("::error::[GD Editor] ", s) # GitHub Action syntax
	get_tree().quit(1)
