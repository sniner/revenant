#!/usr/bin/env bash
#
# create-test-vm.sh — Build a minimal Arch Linux QEMU image for revenant tests.
#
# The image contains:
#   - GPT partition table
#   - EFI partition (512 MiB, vfat), mounted at /boot
#   - Btrfs partition with subvolumes:
#       @       → root filesystem (/)
#       @boot   → EFI staging subvolume (for revenant)
#       @home   → /home
#   - systemd-boot as bootloader
#   - SSH access: root / revenant (guest port 22 → host port 2222)
#
# Usage:
#   sudo ./tools/create-test-vm.sh [image-file]
#
# Default: revenant-test.img in the current directory.

set -euo pipefail

. "$(dirname "$0")/lib-vm.sh"

# ── Configuration ─────────────────────────────────────────────────────────────

IMAGE="${1:-$REVENANT_VM_IMAGE_DEFAULT}"
IMAGE_SIZE="10G"
MNT_TOPLEVEL="/tmp/revenant-vm-toplevel"
MNT_ROOT="/tmp/revenant-vm-root"
PACKAGES="base linux linux-firmware btrfs-progs dosfstools sudo openssh rsync"

LOOP_DEV=""

cleanup() {
    info "cleaning up"
    umount -R "$MNT_ROOT"     2>/dev/null || true
    umount    "$MNT_TOPLEVEL" 2>/dev/null || true
    if [[ -n "$LOOP_DEV" ]]; then
        losetup -d "$LOOP_DEV" 2>/dev/null || true
    fi
    rmdir "$MNT_TOPLEVEL" "$MNT_ROOT" 2>/dev/null || true
}
trap cleanup EXIT

# ── Preconditions ────────────────────────────────────────────────────────────

if [[ $EUID -ne 0 ]]; then
    die "this script must be run as root"
fi

require_tools \
    qemu-img qemu-system-x86_64 \
    parted mkfs.fat mkfs.btrfs \
    pacstrap arch-chroot genfstab
require_ovmf_vars_src

if [[ -e "$IMAGE" ]]; then
    die "image file '$IMAGE' already exists — delete it first"
fi

# ── Image and partitions ──────────────────────────────────────────────────────

info "creating disk image: $IMAGE ($IMAGE_SIZE)"
truncate -s "$IMAGE_SIZE" "$IMAGE"

info "setting up loop device"
LOOP_DEV=$(losetup -fP --show "$IMAGE")
info "loop device: $LOOP_DEV"

info "partitioning (GPT: EFI 512 MiB + btrfs rest)"
parted -s "$LOOP_DEV" mklabel gpt
parted -s "$LOOP_DEV" mkpart ESP fat32 1MiB 513MiB
parted -s "$LOOP_DEV" set 1 esp on
parted -s "$LOOP_DEV" mkpart primary btrfs 513MiB 100%

# The kernel needs a moment to see the new partition nodes.
udevadm settle || sleep 1

EFI_PART="${LOOP_DEV}p1"
BTRFS_PART="${LOOP_DEV}p2"

info "formatting EFI partition (vfat)"
mkfs.fat -F32 -n EFI "$EFI_PART"

info "formatting btrfs partition"
mkfs.btrfs -L arch "$BTRFS_PART"

# ── Btrfs subvolumes ──────────────────────────────────────────────────────────

info "creating btrfs subvolumes (@, @boot, @home)"
mkdir -p "$MNT_TOPLEVEL"
mount -o subvolid=5 "$BTRFS_PART" "$MNT_TOPLEVEL"
btrfs subvolume create "${MNT_TOPLEVEL}/@"
btrfs subvolume create "${MNT_TOPLEVEL}/@boot"   # EFI staging for revenant
btrfs subvolume create "${MNT_TOPLEVEL}/@home"
umount "$MNT_TOPLEVEL"
rmdir  "$MNT_TOPLEVEL"

# ── Mount for installation ────────────────────────────────────────────────────

info "mounting filesystem for installation"
mkdir -p "$MNT_ROOT"
mount -o subvol=@ "$BTRFS_PART" "$MNT_ROOT"
mkdir -p "${MNT_ROOT}/boot"
mount "$EFI_PART" "${MNT_ROOT}/boot"

# ── Install Arch Linux ────────────────────────────────────────────────────────

info "installing Arch Linux (pacstrap) — this takes a while"
pacstrap -K "$MNT_ROOT" $PACKAGES

info "generating fstab"
genfstab -U "$MNT_ROOT" >> "${MNT_ROOT}/etc/fstab"

# ── System configuration (in chroot) ─────────────────────────────────────────

info "configuring system inside chroot"

BTRFS_UUID=$(blkid -s UUID -o value "$BTRFS_PART")

arch-chroot "$MNT_ROOT" /bin/bash -s <<CHROOT
set -euo pipefail

# Locale
echo "en_US.UTF-8 UTF-8" > /etc/locale.gen
locale-gen
echo "LANG=en_US.UTF-8" > /etc/locale.conf

# Hostname
echo "revenant-test" > /etc/hostname

# Root password
echo "root:$REVENANT_VM_SSH_PASS" | chpasswd

# mkinitcpio: include btrfs module
sed -i 's/^MODULES=.*/MODULES=(btrfs)/' /etc/mkinitcpio.conf
mkinitcpio -P

# systemd-boot
bootctl install

# Boot entry
mkdir -p /boot/loader/entries
cat > /boot/loader/entries/arch.conf <<EOF
title   Arch Linux (revenant test)
linux   /vmlinuz-linux
initrd  /initramfs-linux.img
options root=UUID=${BTRFS_UUID} rootflags=subvol=@ rw
EOF

# loader.conf
cat > /boot/loader/loader.conf <<EOF
default arch.conf
timeout 3
console-mode auto
EOF

# SSH: enable and allow root login
systemctl enable sshd
sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config

# Network: DHCP on all ethernet interfaces
cat > /etc/systemd/network/20-ethernet.network <<EOF
[Match]
Name=en*

[Network]
DHCP=yes
EOF
systemctl enable systemd-networkd
systemctl enable systemd-resolved

CHROOT

# ── Copy OVMF vars template ───────────────────────────────────────────────────

VARS_DST=$(vm_vars_file "$IMAGE")
info "copying OVMF vars to $VARS_DST"
cp "$OVMF_VARS_SRC" "$VARS_DST"

# ── Done ──────────────────────────────────────────────────────────────────────

info ""
info "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
info "VM image created: $IMAGE"
info "OVMF vars:        $VARS_DST"
info ""
info "start VM:"
info "  ./tools/start-test-vm.sh $IMAGE"
info ""
info "SSH access (after boot):"
info "  ssh -p $REVENANT_VM_SSH_PORT $REVENANT_VM_SSH_USER@localhost"
info "  password: $REVENANT_VM_SSH_PASS"
info "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
