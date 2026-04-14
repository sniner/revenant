#!/usr/bin/env bash
#
# run-vm-tests.sh — End-to-end VM harness for revenant.
#
# Drives a QEMU VM (created by tools/create-test-vm.sh) through a series
# of scenarios that exercise snapshot/restore including the reboot cycle.
# All VM writes land on a qcow2 overlay that is deleted on exit, so the
# base image is not modified.
#
# Requirements on the host:
#   - qemu-system-x86_64, qemu-img
#   - OVMF firmware (edk2-ovmf package on Arch)
#   - sshpass, ssh, scp
#   - Base image created by tools/create-test-vm.sh
#   - Built revenantctl binary (cargo build is run automatically unless
#     --no-build is given or REVENANT_BIN is set).
#
# Usage:
#   tools/run-vm-tests.sh [options]
#
# Options:
#   --image PATH       Base VM image (default: revenant-test.img)
#   --bin PATH         revenantctl binary (default: target/debug/revenantctl)
#   --no-build         Skip cargo build
#   --keep-overlay     Keep the qcow2 overlay after exit (for debugging)
#   --only NAME        Run only the scenario whose function is `scenario_NAME`
#   -h | --help        Show this help
#
# Exit codes:
#   0   all scenarios passed
#   1   one or more scenarios failed
#   2   precondition error (missing tool, unbuildable, VM won't start)

set -euo pipefail

. "$(dirname "$0")/lib-vm.sh"

# ── Defaults ──────────────────────────────────────────────────────────────────

BASE_IMAGE="$REVENANT_VM_IMAGE_DEFAULT"
REVENANT_BIN="${REVENANT_BIN:-target/debug/revenantctl}"
DO_BUILD=1
KEEP_OVERLAY=0
ONLY_SCENARIO=""

SSH_PORT="$REVENANT_VM_SSH_PORT"
SSH_USER="$REVENANT_VM_SSH_USER"
SSH_PASS="$REVENANT_VM_SSH_PASS"
SSH_TIMEOUT=120
REBOOT_TIMEOUT=180

# ── Argument parsing ─────────────────────────────────────────────────────────

usage() {
    sed -n '3,/^$/p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --image)         BASE_IMAGE="$2"; shift 2 ;;
        --bin)           REVENANT_BIN="$2"; shift 2 ;;
        --no-build)      DO_BUILD=0; shift ;;
        --keep-overlay)  KEEP_OVERLAY=1; shift ;;
        --only)          ONLY_SCENARIO="$2"; shift 2 ;;
        -h|--help)       usage 0 ;;
        *)               echo "unknown arg: $1" >&2; usage 2 ;;
    esac
done

# ── Scenario-aware fail override ──────────────────────────────────────────────
# The library defines `fail` as a plain printer. Here we wrap it to track
# per-scenario state; these variables are set by `run_scenario`.

fail() {
    echo "${__C_RED}[FAIL]${__C_OFF}  $*"
    SCENARIO_FAILED=1
    FAILED_SCENARIOS+=("$CURRENT_SCENARIO")
}

# ── Preconditions ─────────────────────────────────────────────────────────────

require_tools qemu-system-x86_64 qemu-img sshpass ssh scp
require_ovmf_code

[[ -f "$BASE_IMAGE" ]] \
    || die "base image not found: $BASE_IMAGE (run tools/create-test-vm.sh first)"

BASE_VARS=$(vm_vars_file "$BASE_IMAGE")
[[ -f "$BASE_VARS" ]] || die "OVMF vars file not found: $BASE_VARS"

if [[ "$DO_BUILD" -eq 1 ]]; then
    info "building revenantctl"
    cargo build --bin revenantctl >/dev/null
fi
[[ -x "$REVENANT_BIN" ]]      || die "revenantctl binary not found or not executable: $REVENANT_BIN"

# ── Per-run working files ─────────────────────────────────────────────────────

WORK_DIR="$(mktemp -d -t revenant-vmtest-XXXXXX)"
OVERLAY="$WORK_DIR/overlay.qcow2"
VARS_COPY="$WORK_DIR/vars.fd"
QEMU_PIDFILE="$WORK_DIR/qemu.pid"
QEMU_SERIAL="$WORK_DIR/serial.log"

info "work dir: $WORK_DIR"

cp "$BASE_VARS" "$VARS_COPY"
qemu-img create -f qcow2 -F raw -b "$(realpath "$BASE_IMAGE")" "$OVERLAY" >/dev/null

# ── VM lifecycle ──────────────────────────────────────────────────────────────

SSH_OPTS=(
    -o StrictHostKeyChecking=no
    -o UserKnownHostsFile=/dev/null
    -o LogLevel=ERROR
    -o ConnectTimeout=3
    -p "$SSH_PORT"
)

ssh_exec() {
    sshpass -p "$SSH_PASS" ssh "${SSH_OPTS[@]}" "$SSH_USER@localhost" "$@"
}

ssh_send_file() {
    sshpass -p "$SSH_PASS" scp -q \
        -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR \
        -P "$SSH_PORT" \
        "$1" "$SSH_USER@localhost:$2"
}

wait_for_ssh() {
    local timeout="${1:-$SSH_TIMEOUT}"
    local deadline=$(( SECONDS + timeout ))
    while (( SECONDS < deadline )); do
        if ssh_exec true 2>/dev/null; then return 0; fi
        sleep 2
    done
    return 1
}

start_vm() {
    info "starting VM"
    qemu-system-x86_64 \
        -enable-kvm -cpu host -m 2048 -smp 2 \
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
        -drive "if=pflash,format=raw,file=$VARS_COPY" \
        -drive "file=$OVERLAY,if=virtio,format=qcow2" \
        -netdev "user,id=net0,hostfwd=tcp::${SSH_PORT}-:22" \
        -device virtio-net-pci,netdev=net0 \
        -display none \
        -serial "file:$QEMU_SERIAL" \
        -pidfile "$QEMU_PIDFILE" \
        -daemonize

    info "waiting for SSH (up to ${SSH_TIMEOUT}s)"
    wait_for_ssh "$SSH_TIMEOUT" || die "VM did not become reachable via SSH — see $QEMU_SERIAL"
    info "VM is up"
}

stop_vm() {
    [[ -f "$QEMU_PIDFILE" ]] || return 0
    local pid
    pid=$(cat "$QEMU_PIDFILE")
    if kill -0 "$pid" 2>/dev/null; then
        info "shutting down VM (pid $pid)"
        ssh_exec "poweroff" 2>/dev/null || true
        # Give the guest a chance to halt cleanly.
        local deadline=$(( SECONDS + 30 ))
        while (( SECONDS < deadline )) && kill -0 "$pid" 2>/dev/null; do
            sleep 1
        done
        if kill -0 "$pid" 2>/dev/null; then
            info "guest did not halt, sending SIGTERM"
            kill "$pid" 2>/dev/null || true
            sleep 3
            kill -9 "$pid" 2>/dev/null || true
        fi
    fi
    rm -f "$QEMU_PIDFILE"
}

reboot_vm() {
    info "rebooting VM"
    # `reboot` exits the SSH session before the shell finishes; swallow the
    # resulting connection error.
    ssh_exec "nohup reboot >/dev/null 2>&1 &" || true
    # Wait for SSH to drop first, then come back.
    sleep 5
    wait_for_ssh "$REBOOT_TIMEOUT" || die "VM did not return after reboot — see $QEMU_SERIAL"
    info "VM is back up"
}

cleanup() {
    set +e
    stop_vm
    if (( KEEP_OVERLAY )); then
        info "keeping work dir: $WORK_DIR"
    else
        rm -rf "$WORK_DIR"
    fi
}
trap cleanup EXIT

# ── Guest-side setup ──────────────────────────────────────────────────────────

install_revenant_into_vm() {
    info "copying revenantctl into the VM"
    ssh_send_file "$REVENANT_BIN" /usr/local/bin/revenantctl
    ssh_exec "chmod +x /usr/local/bin/revenantctl"
    local ver
    ver=$(ssh_exec "/usr/local/bin/revenantctl --version" 2>/dev/null || true)
    info "guest reports: ${ver:-<unknown>}"
}

# ── Scenarios ─────────────────────────────────────────────────────────────────
#
# Each scenario is a function `scenario_<name>`. It runs commands via
# ssh_exec, sets SCENARIO_FAILED via `fail` on assertion failure, and
# returns. `run_scenario` wraps each with header, timing, and summary.

CURRENT_SCENARIO=""
SCENARIO_FAILED=0
FAILED_SCENARIOS=()
SCENARIOS_RUN=0

run_scenario() {
    local name="$1"
    if [[ -n "$ONLY_SCENARIO" && "$name" != "$ONLY_SCENARIO" ]]; then
        return
    fi
    CURRENT_SCENARIO="$name"
    SCENARIO_FAILED=0
    SCENARIOS_RUN=$((SCENARIOS_RUN + 1))
    local start=$SECONDS
    echo
    echo "${__C_YELLOW}══ scenario: $name ══${__C_OFF}"
    "scenario_$name"
    local dur=$(( SECONDS - start ))
    if (( SCENARIO_FAILED )); then
        echo "${__C_RED}── $name failed after ${dur}s ──${__C_OFF}"
    else
        echo "${__C_GREEN}── $name ok after ${dur}s ──${__C_OFF}"
    fi
}

assert_eq() {
    local desc="$1" expected="$2" actual="$3"
    if [[ "$expected" == "$actual" ]]; then
        pass "$desc"
    else
        fail "$desc: expected [$expected], got [$actual]"
    fi
}

assert_ssh_ok() {
    local desc="$1"; shift
    if ssh_exec "$@" >/dev/null 2>&1; then
        pass "$desc"
    else
        fail "$desc: ssh command failed: $*"
    fi
}

assert_ssh_fail() {
    local desc="$1" expected_code="$2"; shift 2
    local code=0
    ssh_exec "$@" >/dev/null 2>&1 || code=$?
    if (( code == expected_code )); then
        pass "$desc (exit $code)"
    else
        fail "$desc: expected exit $expected_code, got $code"
    fi
}

# Extract the snapshot ID (format YYYYMMDD-HHMMSS) from revenantctl JSON.
extract_snap_id() {
    grep -oE '[0-9]{8}-[0-9]{6}' | head -1
}

# -- init smoke --------------------------------------------------------------
scenario_init_smoke() {
    step "revenantctl init writes config.toml"
    ssh_exec "revenantctl init --force" >/dev/null
    assert_ssh_ok "config.toml exists" "test -f /etc/revenant/config.toml"
    assert_ssh_ok "status runs"         "revenantctl status"
    assert_ssh_ok "check runs"          "revenantctl check"
}

# -- restore refusal without --yes -------------------------------------------
scenario_restore_refusal() {
    step "ensure a snapshot exists"
    local id
    id=$(ssh_exec "revenantctl --json snapshot" | extract_snap_id)
    [[ -n "$id" ]] || { fail "could not create baseline snapshot"; return; }

    step "restore without --yes must refuse with exit 1"
    assert_ssh_fail "restore refusal" 1 "revenantctl restore $id"

    step "subvolume @ must be untouched (no DELETE marker yet)"
    local delete_count
    delete_count=$(ssh_exec "btrfs subvolume list / | grep -c -- '-DELETE-' || true")
    assert_eq "no DELETE marker present" "0" "$delete_count"
}

# -- rootfs snapshot/restore round trip across reboot ------------------------
scenario_restore_reboot() {
    step "mark the live system with a file"
    ssh_exec "echo baseline > /root/revenant-test-marker"

    step "take baseline snapshot"
    local baseline
    baseline=$(ssh_exec "revenantctl --json snapshot" | extract_snap_id)
    [[ -n "$baseline" ]] || { fail "no baseline id"; return; }
    info "baseline id: $baseline"

    step "modify the system (marker + stray file)"
    ssh_exec "echo post > /root/revenant-test-marker"
    ssh_exec "touch /root/should-vanish"

    step "restore to baseline with --yes"
    ssh_exec "revenantctl restore $baseline --yes" >/dev/null

    step "reboot"
    reboot_vm

    step "verify marker reverted and stray file gone"
    local marker
    marker=$(ssh_exec "cat /root/revenant-test-marker")
    assert_eq "marker reverted" "baseline" "$marker"
    assert_ssh_fail "stray file absent" 1 "test -e /root/should-vanish"
}

# -- EFI sync: file on ESP reverts across restore+reboot ---------------------
scenario_efi_sync() {
    step "drop a marker file on the ESP"
    ssh_exec "echo before > /boot/revenant-efi-marker"

    step "snapshot with efi sync enabled"
    local pre
    pre=$(ssh_exec "revenantctl --json snapshot" | extract_snap_id)
    [[ -n "$pre" ]] || { fail "no pre-sync snapshot id"; return; }
    info "pre-sync id: $pre"

    step "modify the ESP marker"
    ssh_exec "echo after > /boot/revenant-efi-marker"

    step "restore and reboot"
    ssh_exec "revenantctl restore $pre --yes" >/dev/null
    reboot_vm

    step "verify ESP marker reverted"
    local marker
    marker=$(ssh_exec "cat /boot/revenant-efi-marker")
    assert_eq "ESP marker reverted" "before" "$marker"
}

# -- snapshot metadata sidecar round trip ------------------------------------
#
# Exercises the `--message` / trigger-metadata path end-to-end:
#   1. snapshot --message writes a sidecar and the message surfaces in list --json
#   2. --trigger pacman reads package names from stdin and records them
#   3. deleting a subvol behind its sidecar's back produces an orphaned-sidecar
#      finding in `check`, and `cleanup` removes the stranded file
#
# The sidecar lives in @snapshots which is not mounted on the running system;
# we mount the btrfs toplevel at /mnt/topvol just for the duration of the
# filesystem-level assertions.
scenario_metadata_sidecar() {
    local rootdev
    rootdev=$(ssh_exec "findmnt -no SOURCE /")

    step "snapshot with --message writes sidecar metadata"
    local id
    id=$(ssh_exec "revenantctl --json snapshot --message 'vm scenario marker'" | extract_snap_id)
    [[ -n "$id" ]] || { fail "could not create snapshot with message"; return; }
    info "snapshot id: $id"

    ssh_exec "mkdir -p /mnt/topvol && mount -o subvolid=5 $rootdev /mnt/topvol"

    step "sidecar file exists on disk"
    assert_ssh_ok "sidecar present" \
        "ls /mnt/topvol/@snapshots/@-default-$id.meta.toml"

    step "list --json surfaces the message"
    local json
    json=$(ssh_exec "revenantctl --json list --strain default")
    if echo "$json" | grep -q 'vm scenario marker'; then
        pass "message in JSON list"
    else
        fail "message missing from JSON list: $json"
    fi

    step "simulated pacman trigger captures stdin targets"
    local pac_id
    pac_id=$(ssh_exec "printf 'hello\nworld\n' | revenantctl --json snapshot --strain default --trigger pacman" \
        | extract_snap_id)
    [[ -n "$pac_id" ]] || { fail "could not create pacman-trigger snapshot"; \
        ssh_exec "umount /mnt/topvol" || true; return; }
    local pac_json
    pac_json=$(ssh_exec "revenantctl --json list --strain default")
    if echo "$pac_json" | grep -q '"kind":"pacman"' \
        && echo "$pac_json" | grep -q 'hello' \
        && echo "$pac_json" | grep -q 'world'; then
        pass "pacman trigger targets captured"
    else
        fail "pacman metadata missing from JSON list: $pac_json"
    fi

    step "orphaned sidecar: delete subvol, keep sidecar"
    ssh_exec "btrfs subvolume delete /mnt/topvol/@snapshots/@-default-$id" >/dev/null

    step "check flags the orphaned sidecar"
    local check_out
    check_out=$(ssh_exec "revenantctl check" || true)
    if echo "$check_out" | grep -q 'orphaned-sidecar'; then
        pass "check reports orphaned sidecar"
    else
        fail "check did not flag orphaned sidecar: $check_out"
    fi

    step "cleanup removes the orphaned sidecar"
    ssh_exec "revenantctl cleanup" >/dev/null
    assert_ssh_fail "sidecar file removed" 2 \
        "test -e /mnt/topvol/@snapshots/@-default-$id.meta.toml"

    ssh_exec "umount /mnt/topvol" || true
}

# -- cleanup removes DELETE markers ------------------------------------------
scenario_cleanup_delete_markers() {
    step "expect at least one DELETE marker from prior restore scenarios"
    local before
    before=$(ssh_exec "btrfs subvolume list / | grep -c -- '-DELETE-' || true")
    if (( before == 0 )); then
        info "no DELETE marker present — creating one"
        local id
        id=$(ssh_exec "revenantctl --json snapshot" | extract_snap_id)
        ssh_exec "revenantctl restore $id --yes" >/dev/null
        reboot_vm
        before=$(ssh_exec "btrfs subvolume list / | grep -c -- '-DELETE-' || true")
    fi
    info "DELETE markers before cleanup: $before"

    step "run cleanup and verify markers are gone"
    ssh_exec "revenantctl cleanup" >/dev/null
    local after
    after=$(ssh_exec "btrfs subvolume list / | grep -c -- '-DELETE-' || true")
    assert_eq "DELETE markers removed" "0" "$after"
}

# ── Main ──────────────────────────────────────────────────────────────────────

start_vm
install_revenant_into_vm

run_scenario init_smoke
run_scenario restore_refusal
run_scenario restore_reboot
run_scenario efi_sync
run_scenario metadata_sidecar
run_scenario cleanup_delete_markers

echo
if (( ${#FAILED_SCENARIOS[@]} == 0 )); then
    echo "${__C_GREEN}all $SCENARIOS_RUN scenarios passed${__C_OFF}"
    exit 0
else
    echo "${__C_RED}${#FAILED_SCENARIOS[@]} of $SCENARIOS_RUN scenarios failed:${__C_OFF}"
    for s in "${FAILED_SCENARIOS[@]}"; do echo "  - $s"; done
    exit 1
fi
