# Contributing to revenant

Developer-facing documentation: how to build, test, and navigate the
codebase. End-user documentation lives in [README.md](README.md).

## Requirements

- Linux (revenant is Linux-only by design — systemd-boot, Btrfs, ioctls)
- Rust 1.85 or newer
- For the loopback test suite (optional): `btrfs-progs`, `util-linux`,
  `sudo`, and the loop kernel module

## Building

The repository is a Cargo workspace with three crates:

```
crates/revenant-core/   # library: snapshot logic, backend trait, config
crates/revenant-cli/    # binary `revenantctl`
crates/revenant-gui/    # GUI stub (libadwaita; not yet implemented)
```

Standard Cargo commands work at the workspace root:

```sh
cargo build --all              # debug build
cargo build --all --release    # release build
cargo run -p revenant-cli -- status   # run revenantctl from source
```

The CLI binary is named `revenantctl` and is produced under
`target/<profile>/revenantctl`.

## Code style and lints

Clippy runs in strict mode. Before committing:

```sh
cargo clippy --all --all-targets -- -D warnings
```

This must be clean. The same command runs in CI (once that exists), so
warnings will block merges.

## Running tests

There are two layers of tests, with very different cost and risk
profiles. A planned third layer (full VM end-to-end) is not yet
implemented.

### Layer A — unit and orchestration tests (default)

Plain `cargo test` runs the entire hermetic test suite. No root, no
external tools, no filesystem touched outside the Cargo target dir:

```sh
cargo test --all
```

Layer A covers:

- Snapshot ID parsing and formatting
- Retention policy logic
- Config loading and validation
- Snapshot orchestration (`create_snapshot`, `delete_snapshot`,
  `delete_all_strain`, `discover_snapshots`)
- Restore orchestration (`restore_snapshot`, including the DELETE-marker
  rename, the pre-restore safety snapshot, and incomplete-snapshot
  rejection)
- Cleanup orchestration (`apply_retention`, `purge_delete_pending_all`)
- Health checks (`find_orphaned_snapshots`, `parse_snapshot_name`)
- Systemd unit generation

The orchestration tests run against a `MockBackend` defined under
`crates/revenant-core/src/backend/mock.rs`. This is an in-memory
implementation of the `FileSystemBackend` trait that tracks subvolumes
in a `HashMap` and mirrors btrfs's observable behaviour closely enough
to exercise every code path that touches the filesystem — including
ENOTEMPTY semantics on `delete_subvolume`. It is gated behind
`#[cfg(test)]` so it never ships in release builds.

When you add functionality that calls into the backend, prefer mock-
based tests in Layer A: they are fast, deterministic, and run on every
machine without privileges.

### Layer B — loopback btrfs integration tests

Layer B exercises the real `BtrfsBackend` against an actual btrfs
filesystem mounted from a loopback image. It catches things the mock
cannot, in particular:

- The readonly-flag clearing dance in `delete_subvolume` (btrfs refuses
  to delete a readonly subvolume; the backend must clear `RDONLY` first)
- `find_nested_subvolumes` walking a real directory tree
- Real ioctl semantics for `BTRFS_IOC_SNAP_CREATE_V2`,
  `BTRFS_IOC_SUBVOL_CREATE`, etc.
- End-to-end create-then-restore against real kernel behaviour

These tests are gated behind the `loopback-tests` Cargo feature so
that `cargo test` stays unprivileged and hermetic. They are **not**
run by default.

#### Why you should run Layer B in a VM

Layer B requires root because it calls `mount`, attaches loop
devices, and runs `mkfs.btrfs`. Even with extensive hardening (see
below), running privileged Btrfs operations on a host you care about
is a bad idea. **Always run Layer B in a throwaway VM.**

The hardening is real, but it is defense in depth, not a guarantee:

- **Sandbox prefix.** Every path the fixture creates lives under
  `/tmp/revenant-loopback-test/`. The `assert_sandboxed` helper in
  `crates/revenant-core/tests/common/mod.rs` panics before any
  destructive operation if a path falls outside this prefix.
- **Private mount namespace.** The wrapper script invokes the test
  binary inside `unshare --mount --propagation=private`. Mounts
  performed by the tests are invisible to the host and die with the
  namespace. A crashing test cannot leave stale mounts behind.
- **Auto-allocated loop devices.** `losetup -f --show` lets the kernel
  pick a free loop device. Stale devices from a crashed prior run get
  detached at the start of each test session by the wrapper script,
  but only if their backing file is under the sandbox prefix.
- **Single-threaded execution.** The wrapper script forces
  `--test-threads=1` to avoid races on the global loop device pool.
- **Per-test fixture.** Each test gets its own fresh image, loop
  device, and mount point — there is no shared state to leak between
  tests.
- **`cargo` runs unprivileged.** Only the compiled test binary is run
  as root, never `cargo` itself. `target/` stays user-owned.

What the hardening does *not* protect against: a kernel bug in btrfs
or the loop driver, a host where `unshare` is missing or restricted,
or a typo in this document that you copy-paste blindly. In a VM none
of that matters.

#### Running Layer B

In a fresh VM with `btrfs-progs`, `sudo`, and a Rust toolchain
installed:

```sh
./tools/run-loopback-tests.sh
```

The script:

1. Cleans up any leftover sandbox dirs and stale loop devices from a
   prior crashed run.
2. Builds the test binary as the current user
   (`cargo test --features loopback-tests --test loopback --no-run`).
3. Locates the compiled binary by parsing `cargo`'s JSON output.
4. Runs the binary with `sudo unshare --mount --propagation=private`,
   single-threaded, with `--nocapture`.

Expected output: 17 tests pass in roughly 5–10 seconds (`mkfs.btrfs`
per test dominates).

If you need to invoke the tests manually (e.g. for debugging a single
test), the equivalent command is:

```sh
cargo test --package revenant-core \
           --features loopback-tests \
           --test loopback --no-run
sudo unshare --mount --propagation=private \
    target/debug/deps/loopback-<hash> \
    --test-threads=1 --nocapture <test-name>
```

#### Troubleshooting Layer B

| Symptom | Cause | Fix |
|---|---|---|
| `loopback tests require root privileges` | Test binary started without the wrapper script | Use `tools/run-loopback-tests.sh` |
| `losetup: cannot find an unused loop device` | Loop kernel module not loaded | `sudo modprobe loop` |
| `mkfs.btrfs: command not found` | btrfs userspace tools missing | Install `btrfs-progs` |
| Tests hang | Kernel issue or stuck mount | `Ctrl-C`; the private mount namespace dies with the unshare process. If `/tmp/revenant-loopback-test` is non-empty afterwards: `sudo losetup -D && sudo rm -rf /tmp/revenant-loopback-test` |

### Layer C — VM end-to-end tests (planned)

Not yet implemented. The intent is a small Arch Linux VM image that
boots, runs `revenantctl init` + `snapshot` + `restore`, reboots into
the restored state, and asserts the expected file content survived.
Tracked as a checklist item in the README.

## Repository layout

```
crates/
  revenant-core/        Library: all snapshot logic
    src/
      backend/          FileSystemBackend trait + BtrfsBackend
        btrfs/          ioctl wrappers (no external btrfs binary)
        mock.rs         In-memory backend for unit tests (cfg(test))
      bootloader/       systemd-boot integration
      check.rs          Health checks (orphans, nested subvols)
      cleanup.rs        Retention policy application
      config.rs         TOML config loading
      efi.rs            EFI/ESP rsync-style sync
      error.rs          RevenantError + Result alias
      init.rs           System auto-detection from /proc/self/mountinfo
      restore.rs        restore_snapshot orchestration
      retention.rs      select_to_keep (pure function)
      snapshot.rs       create/delete/discover snapshots
      systemd.rs        systemd unit file generation
    tests/
      common/mod.rs     TestFs RAII fixture for loopback tests
      loopback.rs       Layer B integration tests (cfg(loopback-tests))
  revenant-cli/         Binary crate: revenantctl CLI
  revenant-gui/         GUI stub (libadwaita, not yet implemented)
tools/
  run-loopback-tests.sh Wrapper for Layer B tests
```

### Architectural principles

- **Backend abstraction.** All COW filesystem operations go through the
  `FileSystemBackend` trait. The library never calls `std::fs::rename`
  or btrfs ioctls directly; everything goes through the trait so it can
  be mocked.
- **Subvolume existence checks.** Use the `subvol_exists(backend, path)`
  helper from `backend/mod.rs` rather than `path.exists()`. The latter
  is true for any directory and is also not mockable.
- **Snapshot naming is the source of truth.** A snapshot's identity
  lives entirely in its subvolume name (`{subvol}-{strain}-{timestamp}`).
  No sidecar database, no metadata file. Strain names are restricted to
  `[a-zA-Z0-9_]` so the rightmost dash before the timestamp
  unambiguously separates `{subvol}` from `{strain}`.
- **No external binaries at runtime.** Btrfs operations call ioctls
  directly so revenantctl works in a minimal recovery environment.
  The only external dependency is glibc.

## Adding a new filesystem backend

1. Implement the `FileSystemBackend` trait
   (`crates/revenant-core/src/backend/mod.rs`) for your backend type.
2. Mirror the safety semantics of `BtrfsBackend`: refuse to delete a
   non-empty subvolume, return `SubvolumeNotFound` for missing paths,
   etc. The mock backend (`backend/mock.rs`) is the closest thing to
   a behavioural spec.
3. Wire the backend into config-driven selection in
   `crates/revenant-core/src/config.rs`.
4. Add Layer A tests against your new backend through the existing
   orchestration tests — most of them are written against the trait,
   not the concrete type.
5. If your backend has filesystem-specific quirks (like the btrfs
   readonly-flag dance), add a Layer B integration test.

## Release process

Not yet defined. Planned: tagged releases, statically linked musl
binaries built in CI, attached to GitHub release pages.
