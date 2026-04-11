#!/bin/sh
# Run the revenant-core loopback btrfs integration tests safely.
#
# This script:
#   1. Cleans up any leftover sandbox dirs and loop devices from a
#      previous crashed run (only paths under /tmp/revenant-loopback-test).
#   2. Builds the test binary as the current user (no sudo for cargo,
#      so target/ stays user-owned).
#   3. Locates the compiled test binary from cargo's JSON output.
#   4. Runs the binary as root inside a private mount namespace
#      (`unshare --mount --propagation=private`) so that any mounts the
#      tests perform die with the namespace and cannot leak to the host.
#   5. Forces single-threaded execution so concurrent tests don't race
#      on shared loop devices.
#
# WARNING: only run this on a system where root processes are allowed
# to mount filesystems and create loop devices. The fixture is hardened
# but still touches privileged kernel interfaces.

set -eu

cd "$(dirname "$0")/.."

SANDBOX_PREFIX="/tmp/revenant-loopback-test"

# --- Step 1: best-effort cleanup of stale state ---
# Detach any loop devices whose backing file is under our sandbox prefix.
if command -v losetup >/dev/null 2>&1; then
    sudo losetup -a 2>/dev/null \
        | awk -F: -v prefix="$SANDBOX_PREFIX" '
            $0 ~ prefix { print $1 }
        ' \
        | while read -r dev; do
            echo "cleanup: detaching stale loop device $dev"
            sudo losetup -d "$dev" 2>/dev/null || true
        done
fi
# Remove the sandbox tree itself. The hard-coded prefix is the safety
# net here — never make this path dynamic.
if [ -d "$SANDBOX_PREFIX" ]; then
    sudo rm -rf "$SANDBOX_PREFIX"
fi

# --- Step 2: build (as current user) ---
echo "Building loopback test binary..."
cargo test \
    --package revenant-core \
    --features loopback-tests \
    --test loopback \
    --no-run \
    >/dev/null

# --- Step 3: locate the binary via JSON output ---
BIN=$(cargo test \
        --package revenant-core \
        --features loopback-tests \
        --test loopback \
        --no-run \
        --message-format=json 2>/dev/null \
    | grep -oE '"executable":"[^"]*/loopback-[a-f0-9]+"' \
    | sed -E 's/"executable":"([^"]+)"/\1/' \
    | head -n1)

if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
    echo "error: could not locate compiled loopback test binary" >&2
    exit 1
fi
echo "Test binary: $BIN"

# --- Step 4: run as root in a private mount namespace ---
# Single-threaded to avoid races on the global loop device pool, and
# --nocapture so failures show the assertion text directly. We do NOT
# exec here so that we get a chance to clean up the sandbox parent
# directory afterwards (TestFs::Drop only removes per-test subdirs).
echo "Running tests in private mount namespace (sudo will prompt)..."
set +e
sudo unshare --mount --propagation=private \
    "$BIN" --test-threads=1 --nocapture "$@"
TEST_EXIT=$?
set -e

# --- Step 5: post-run cleanup of the sandbox parent dir ---
# Hard-coded prefix is the safety net; never make this dynamic.
sudo rm -rf "$SANDBOX_PREFIX" 2>/dev/null || true

exit $TEST_EXIT
