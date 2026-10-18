#!/usr/bin/env bash
# Copyright (c) godot-rust; Bromeon and contributors.
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

rel="."

# If argument is 'api-custom', set 'cargoArgs' to use that feature. If 'stable', set 'cargoArgs' to empty string. Otherwise, error.
if [[ $1 == "api-custom" ]]; then
  cargoArgs="--features godot/api-custom"
elif [[ $1 == "stable" ]]; then
  cargoArgs=""
elif [[ $1 == api-4-* ]]; then
  cargoArgs="--features godot/$1"
else
  echo "[Bash]      Error: Unknown argument '$1'. Expected 'stable', 'api-custom' or 'api-4.*'."
  exit 1
fi

if [[ $2 == "signal-test" ]]; then
    cargoArgs="${cargoArgs} --features signal-test"
fi

# Restore un-reloaded files on exit (for local testing).
cleanedUp=0 # avoid recursion if cleanup fails
godotPid=0 # kill previous instance if necessary
threadedReleaseFile="$PWD/.threaded_reload_release"
threadedStartedFile="$PWD/.threaded_reload_started"
threadedCompletedFile="$PWD/.threaded_reload_completed"
threadedTimeoutFile="$PWD/.threaded_reload_timeout"

cleanup() {
  if [[ $cleanedUp -eq 0 ]]; then
    cleanedUp=1
    if [[ $godotPid -ne 0 ]] && kill -0 "$godotPid" 2>/dev/null; then
      echo "[Bash]      Kill Godot (PID $godotPid)..."
      kill "$godotPid" 2>/dev/null || true # ignore errors here
      wait "$godotPid" 2>/dev/null || true
      godotPid=0
    fi
    echo "[Bash]      Cleanup..."
    rm -f "$threadedReleaseFile" "$threadedStartedFile" "$threadedCompletedFile" "$threadedTimeoutFile"
    git checkout --quiet $rel/../rust/src/lib.rs $rel/rust.gdextension $rel/MainScene.tscn || true # ignore errors here
  fi
}

set -euo pipefail
trap cleanup EXIT

# Set up Godot binary env var, supporting both old and new names.
GODOT_BIN="${GDRUST_GODOT_BIN:-${GODOT4_BIN:-}}"
if [[ -z "$GODOT_BIN" ]]; then
  echo "[Bash]      Error: Neither GDRUST_GODOT_BIN nor GODOT4_BIN is set."
  exit 1
fi
if [[ -n "${GODOT4_BIN:-}" ]] && [[ -z "${GDRUST_GODOT_BIN:-}" ]]; then
  echo "[Bash]      Warning: GODOT4_BIN is deprecated, use GDRUST_GODOT_BIN instead."
fi

godotAwait() {
  if [[ $godotPid -ne 0 ]]; then
    echo "[Bash]      Error: godotAwait called while Godot (PID $godotPid) is still running."
    exit 1
  fi

  rm -f "$threadedReleaseFile" "$threadedStartedFile" "$threadedCompletedFile" "$threadedTimeoutFile"

  if [[ "${GDRUST_GDB:-0}" == "1" ]]; then
    gdb --batch --return-child-result \
      -ex "set pagination off" \
      -ex "run" \
      -ex "frame 1" \
      -ex "print this->_instance_binding_count" \
      -ex "print *this->_instance_bindings@this->_instance_binding_count" \
      -ex "info sharedlibrary" \
      -ex "thread apply all bt full" \
      --args "$GODOT_BIN" -e --headless --path "$rel" &
  else
    "$GODOT_BIN" -e --headless --path "$rel" &
  fi
  godotPid=$!
  echo "[Bash]      Wait for Godot ready (PID $godotPid)..."

  $GODOT_BIN --headless --no-header --script ReloadOrchestrator.gd -- await
}

godotNotify() {
  if [[ $godotPid -eq 0 ]]; then
    echo "[Bash]      Error: godotNotify called but Godot is not running."
    exit 1
  fi

  $GODOT_BIN --headless --no-header --script ReloadOrchestrator.gd -- notify

  echo "[Bash]      Wait for reload_extension() to enter, then release worker callback..."
  local deadline=$((SECONDS + 10))
  while [[ ! -f "$threadedStartedFile" ]]; do
    if (( SECONDS >= deadline )); then
      echo "[Bash]      Error: timed out waiting for threaded reload start marker."
      exit 1
    fi
    sleep 0.02
  done

  # Leave a visible interval during which reload_extension() must be blocked in callback drain.
  sleep 0.4
  if [[ -f "$threadedCompletedFile" ]]; then
    echo "[Bash]      Error: worker callback completed before explicit release."
    exit 1
  fi
  if ! kill -0 "$godotPid" 2>/dev/null; then
    echo "[Bash]      Error: Godot exited while reload should be waiting for callback drain."
    exit 1
  fi
  touch "$threadedReleaseFile"

  echo "[Bash]      Wait for Godot exit..."
  local status=0
  if wait "$godotPid"; then
    status=0
  else
    status=$?
  fi
  echo "[Bash]      Godot (PID $godotPid) has completed with status $status."
  godotPid=0

  if [[ $status -ne 0 ]]; then
    exit $status
  fi
}

echo "[Bash]      Start hot-reload integration test..."

# Restore un-reloaded file (for local testing).
git checkout --quiet $rel/../rust/src/lib.rs $rel/rust.gdextension

# Set up editor file which has scene open, so @tool script loads at startup. Also copy scene file that holds a script.
mkdir -p $rel/.godot/editor
cp editor_layout.cfg $rel/.godot/editor/editor_layout.cfg
#cp MainScene.tscn $rel/MainScene.tscn

# Compile original Rust source.
#cargoArgs="--features godot/__debug-log"
cargo build -p hot-reload $cargoArgs

# Wait briefly so artifacts are present on file system.
sleep 0.5

# ----------------------------------------------------------------
# Test Case 1: Update Rust source and compile to trigger reload.
# ----------------------------------------------------------------

echo "[Bash]      Scenario 1: Reload after updating Rust source..."

godotAwait
$GODOT_BIN --headless --no-header --script ReloadOrchestrator.gd -- replace
# Compile updated Rust source.
cargo build -p hot-reload $cargoArgs
godotNotify

# ----------------------------------------------------------------
# Test Case 2: Touch the .gdextension file to trigger reload.
# ----------------------------------------------------------------

echo "[Bash]      Scenario 2: Reload after touching rust.gdextension..."

godotAwait
# Update timestamp to trigger reload.
touch "$rel/rust.gdextension"
godotNotify
