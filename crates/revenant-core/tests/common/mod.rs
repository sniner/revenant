//! Loopback btrfs test fixture.
//!
//! Each `TestFs::new()` builds a fresh sparse image, runs `mkfs.btrfs` on
//! it, attaches a loop device with `losetup -f --show`, and mounts the
//! result under a unique sandbox directory. `Drop` reverses every step.
//!
//! Hard safety rule: every path this module touches must live under
//! [`SANDBOX_PREFIX`]. The `assert_sandboxed` helper enforces this before
//! any destructive operation. Combined with the private mount namespace
//! created by `tools/run-loopback-tests.sh`, this means a stray bug in
//! the fixture cannot affect the host filesystem.
//!
//! Cleanup of stale state from a previously crashed run is the wrapper
//! script's job, not this module's — by the time `TestFs::new` is called
//! we trust that the namespace is empty.

#![cfg(feature = "loopback-tests")]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Hard-coded sandbox prefix. Anything outside this tree is off-limits.
pub const SANDBOX_PREFIX: &str = "/tmp/revenant-loopback-test";

/// Image size for each test filesystem. btrfs needs ~110 MiB minimum;
/// 300 MiB gives plenty of headroom for snapshot tests without being slow.
const IMAGE_SIZE: u64 = 300 * 1024 * 1024;

/// RAII fixture wrapping a loop-mounted btrfs filesystem.
pub struct TestFs {
    /// Mount point — pass this to backend operations.
    pub mount: PathBuf,
    sandbox: PathBuf,
    loop_dev: String,
}

impl TestFs {
    pub fn new() -> Self {
        assert!(
            is_root(),
            "loopback tests require root privileges; \
             run via tools/run-loopback-tests.sh"
        );

        // Ensure the sandbox parent exists, then mint a unique subdir.
        std::fs::create_dir_all(SANDBOX_PREFIX).expect("create sandbox parent dir");
        let sandbox = mktemp_dir(SANDBOX_PREFIX, "fs-");
        assert_sandboxed(&sandbox);

        let image = sandbox.join("image.btrfs");
        let mount = sandbox.join("mnt");
        std::fs::create_dir(&mount).expect("create mount dir");

        // Sparse image of fixed size.
        let f = std::fs::File::create(&image).expect("create image file");
        f.set_len(IMAGE_SIZE).expect("set image size");
        drop(f);

        // mkfs.btrfs on the regular file (no loop device needed for mkfs).
        run("mkfs.btrfs", &["-q", "-f", image.to_str().unwrap()]);

        // Attach a loop device. -f picks a free one, --show prints its name.
        let loop_dev = run_capture("losetup", &["-f", "--show", image.to_str().unwrap()])
            .trim()
            .to_string();
        assert!(
            loop_dev.starts_with("/dev/loop"),
            "unexpected losetup output: {loop_dev}"
        );

        // Mount the loop device on the sandboxed mount point.
        run(
            "mount",
            &["-t", "btrfs", &loop_dev, mount.to_str().unwrap()],
        );

        TestFs {
            mount,
            sandbox,
            loop_dev,
        }
    }
}

impl Drop for TestFs {
    fn drop(&mut self) {
        // Belt and braces: re-verify the sandbox path before any rm.
        assert_sandboxed(&self.sandbox);

        // Best-effort teardown — log but never panic from Drop.
        let _ = Command::new("umount").arg(&self.mount).status();
        let _ = Command::new("losetup")
            .args(["-d", &self.loop_dev])
            .status();
        if let Err(e) = std::fs::remove_dir_all(&self.sandbox) {
            eprintln!(
                "TestFs cleanup: failed to remove {}: {e}",
                self.sandbox.display()
            );
        }
    }
}

/// Panic if `path` is not under [`SANDBOX_PREFIX`].
fn assert_sandboxed(path: &Path) {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    assert!(
        canon.starts_with(SANDBOX_PREFIX),
        "refusing to operate on path outside sandbox: {} (canonical: {})",
        path.display(),
        canon.display(),
    );
}

fn is_root() -> bool {
    // SAFETY: geteuid is always safe to call.
    unsafe { libc::geteuid() == 0 }
}

fn mktemp_dir(parent: &str, prefix: &str) -> PathBuf {
    let template = format!("{parent}/{prefix}XXXXXX");
    let out = run_capture("mktemp", &["-d", &template]);
    PathBuf::from(out.trim())
}

fn run(cmd: &str, args: &[&str]) {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn {cmd}: {e}"));
    assert!(status.success(), "{cmd} {args:?} exited with {status}");
}

fn run_capture(cmd: &str, args: &[&str]) -> String {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {cmd}: {e}"));
    assert!(
        out.status.success(),
        "{cmd} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("non-utf8 output")
}
