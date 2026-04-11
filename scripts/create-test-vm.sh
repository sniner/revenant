#!/usr/bin/env bash
#
# create-test-vm.sh — Erstellt ein QEMU-VM-Image mit minimalem Arch Linux
# für das Testen von revenant.
#
# Das Image enthält:
#   - GPT-Partitionstabelle
#   - EFI-Partition (512 MiB, vfat), gemountet als /boot
#   - Btrfs-Partition mit Subvolumes:
#       @       → Root-Filesystem (/)
#       @boot   → EFI-Staging-Subvolume (für revenant)
#   - systemd-boot als Bootloader
#   - SSH-Zugang: root / revenant (Port 22 in der VM → 2222 am Host)
#
# Aufruf:
#   sudo ./scripts/create-test-vm.sh [image-datei]
#
# Standard: revenant-test.img im aktuellen Verzeichnis

set -euo pipefail

# ── Konfiguration ─────────────────────────────────────────────────────────────

IMAGE="${1:-revenant-test.img}"
IMAGE_SIZE="10G"
MNT_TOPLEVEL="/tmp/revenant-vm-toplevel"
MNT_ROOT="/tmp/revenant-vm-root"
PACKAGES="base linux linux-firmware btrfs-progs dosfstools sudo openssh rsync"

OVMF_VARS_SRC="/usr/share/edk2/x64/OVMF_VARS.4m.fd"

# ── Hilfsfunktionen ───────────────────────────────────────────────────────────

info()  { echo "[INFO]  $*"; }
error() { echo "[ERROR] $*" >&2; }

LOOP_DEV=""

cleanup() {
    info "Aufräumen ..."
    umount -R "$MNT_ROOT"     2>/dev/null || true
    umount    "$MNT_TOPLEVEL" 2>/dev/null || true
    if [[ -n "$LOOP_DEV" ]]; then
        losetup -d "$LOOP_DEV" 2>/dev/null || true
    fi
    rmdir "$MNT_TOPLEVEL" "$MNT_ROOT" 2>/dev/null || true
}

trap cleanup EXIT

check_deps() {
    local missing=()
    local tools=(
        qemu-img
        qemu-system-x86_64
        parted
        mkfs.fat
        mkfs.btrfs
        pacstrap
        arch-chroot
        genfstab
    )
    for t in "${tools[@]}"; do
        command -v "$t" &>/dev/null || missing+=("$t")
    done
    if [[ ! -f "$OVMF_VARS_SRC" ]]; then
        missing+=("edk2-ovmf (${OVMF_VARS_SRC} fehlt)")
    fi
    if [[ ${#missing[@]} -gt 0 ]]; then
        error "Fehlende Abhängigkeiten:"
        for m in "${missing[@]}"; do
            error "  - $m"
        done
        error ""
        error "Installieren mit: pacman -S qemu-base arch-install-scripts btrfs-progs dosfstools edk2-ovmf"
        exit 1
    fi
}

# ── Voraussetzungen prüfen ────────────────────────────────────────────────────

if [[ $EUID -ne 0 ]]; then
    error "Dieses Script muss als root ausgeführt werden."
    exit 1
fi

check_deps

if [[ -e "$IMAGE" ]]; then
    error "Image-Datei '${IMAGE}' existiert bereits. Bitte zuerst löschen."
    exit 1
fi

# ── Image und Partitionen ─────────────────────────────────────────────────────

info "Erstelle Disk-Image: ${IMAGE} (${IMAGE_SIZE})"
truncate -s "$IMAGE_SIZE" "$IMAGE"

info "Richte Loop-Device ein ..."
LOOP_DEV=$(losetup -fP --show "$IMAGE")
info "Loop-Device: ${LOOP_DEV}"

info "Partitioniere (GPT: EFI 512 MiB + Btrfs Rest) ..."
parted -s "$LOOP_DEV" mklabel gpt
parted -s "$LOOP_DEV" mkpart ESP fat32 1MiB 513MiB
parted -s "$LOOP_DEV" set 1 esp on
parted -s "$LOOP_DEV" mkpart primary btrfs 513MiB 100%

# Kernel braucht einen Moment, um die neuen Partition-Nodes zu sehen
udevadm settle || sleep 1

EFI_PART="${LOOP_DEV}p1"
BTRFS_PART="${LOOP_DEV}p2"

info "Formatiere EFI-Partition (vfat) ..."
mkfs.fat -F32 -n EFI "$EFI_PART"

info "Formatiere Btrfs-Partition ..."
mkfs.btrfs -L arch "$BTRFS_PART"

# ── Btrfs-Subvolumes ──────────────────────────────────────────────────────────

info "Erstelle Btrfs-Subvolumes (@, @boot) ..."
mkdir -p "$MNT_TOPLEVEL"
mount -o subvolid=5 "$BTRFS_PART" "$MNT_TOPLEVEL"
btrfs subvolume create "${MNT_TOPLEVEL}/@"
btrfs subvolume create "${MNT_TOPLEVEL}/@boot"   # EFI-Staging für revenant
btrfs subvolume create "${MNT_TOPLEVEL}/@home"
umount "$MNT_TOPLEVEL"
rmdir  "$MNT_TOPLEVEL"

# ── Mounten für Installation ──────────────────────────────────────────────────

info "Mounte Filesystem für Installation ..."
mkdir -p "$MNT_ROOT"
mount -o subvol=@ "$BTRFS_PART" "$MNT_ROOT"
mkdir -p "${MNT_ROOT}/boot"
mount "$EFI_PART" "${MNT_ROOT}/boot"

# ── Arch Linux installieren ───────────────────────────────────────────────────

info "Installiere Arch Linux (pacstrap) — das dauert eine Weile ..."
pacstrap -K "$MNT_ROOT" $PACKAGES

info "Generiere fstab ..."
genfstab -U "$MNT_ROOT" >> "${MNT_ROOT}/etc/fstab"

# ── System konfigurieren (in chroot) ─────────────────────────────────────────

info "Konfiguriere System im chroot ..."

# UUID der Btrfs-Partition für den Boot-Entry ermitteln
BTRFS_UUID=$(blkid -s UUID -o value "$BTRFS_PART")

arch-chroot "$MNT_ROOT" /bin/bash -s <<CHROOT
set -euo pipefail

# Locale
echo "en_US.UTF-8 UTF-8" > /etc/locale.gen
locale-gen
echo "LANG=en_US.UTF-8" > /etc/locale.conf

# Hostname
echo "revenant-test" > /etc/hostname

# Root-Passwort
echo "root:revenant" | chpasswd

# mkinitcpio: btrfs als Modul einbinden
sed -i 's/^MODULES=.*/MODULES=(btrfs)/' /etc/mkinitcpio.conf
mkinitcpio -P

# systemd-boot installieren
bootctl install

# Boot-Entry
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

# SSH aktivieren und Root-Login erlauben
systemctl enable sshd
sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config

# Netzwerk: DHCP auf allen Ethernet-Interfaces
cat > /etc/systemd/network/20-ethernet.network <<EOF
[Match]
Name=en*

[Network]
DHCP=yes
EOF
systemctl enable systemd-networkd
systemctl enable systemd-resolved

CHROOT

# ── OVMF-Variablen-Datei kopieren ─────────────────────────────────────────────

VARS_DST="${IMAGE%.img}-vars.fd"
info "Kopiere OVMF_VARS nach ${VARS_DST} ..."
cp "$OVMF_VARS_SRC" "$VARS_DST"

# ── Fertig ────────────────────────────────────────────────────────────────────

info ""
info "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
info "VM-Image erfolgreich erstellt: ${IMAGE}"
info "OVMF-Vars:                     ${VARS_DST}"
info ""
info "VM starten:"
info "  ./scripts/start-test-vm.sh ${IMAGE}"
info ""
info "SSH-Zugang (nach dem Boot):"
info "  ssh -p 2222 root@localhost"
info "  Passwort: revenant"
info "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
