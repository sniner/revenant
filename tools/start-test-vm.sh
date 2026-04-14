#!/usr/bin/env bash
#
# start-test-vm.sh — Launch the revenant test VM with QEMU/KVM.
#
# Usage:
#   ./tools/start-test-vm.sh [image-file]
#
# Default: revenant-test.img in the current directory.
#
# SSH access after boot:
#   ssh -p 2222 root@localhost   (password: revenant)
#
# Copy the revenant binary into the VM:
#   scp -P 2222 target/debug/revenantctl root@localhost:/usr/local/bin/
#
# Stop the VM:
#   from inside: poweroff
#   from outside: Ctrl+A X (works because of -nographic)

set -euo pipefail

. "$(dirname "$0")/lib-vm.sh"

IMAGE="${1:-$REVENANT_VM_IMAGE_DEFAULT}"
VARS=$(vm_vars_file "$IMAGE")

[[ -f "$IMAGE" ]] || die "image file '$IMAGE' not found — run: sudo ./tools/create-test-vm.sh"
[[ -f "$VARS"  ]] || die "OVMF vars '$VARS' not found — run: sudo ./tools/create-test-vm.sh"
require_ovmf_code

info "starting VM: $IMAGE"
info "SSH:          ssh -p $REVENANT_VM_SSH_PORT $REVENANT_VM_SSH_USER@localhost"
info "stop:         poweroff (inside VM) or Ctrl+A X"
echo

exec qemu-system-x86_64 \
    -enable-kvm \
    -m 2048 \
    -cpu host \
    -smp 2 \
    -drive "file=${IMAGE},format=raw,if=virtio" \
    -drive "if=pflash,format=raw,readonly=on,file=${OVMF_CODE}" \
    -drive "if=pflash,format=raw,file=${VARS}" \
    -netdev "user,id=net0,hostfwd=tcp::${REVENANT_VM_SSH_PORT}-:22" \
    -device "virtio-net-pci,netdev=net0" \
    -nographic
