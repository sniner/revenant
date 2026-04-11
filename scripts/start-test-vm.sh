#!/usr/bin/env bash
#
# start-test-vm.sh — Startet die revenant-Test-VM mit QEMU/KVM
#
# Aufruf:
#   ./scripts/start-test-vm.sh [image-datei]
#
# Standard: revenant-test.img im aktuellen Verzeichnis
#
# SSH-Zugang nach dem Boot:
#   ssh -p 2222 root@localhost   (Passwort: revenant)
#
# revenant-Binary in die VM kopieren:
#   scp -P 2222 target/debug/revenantctl root@localhost:/usr/local/bin/
#
# VM beenden:
#   In der VM: poweroff
#   Oder: Ctrl+A X (in -nographic Modus)

set -euo pipefail

IMAGE="${1:-revenant-test.img}"
VARS="${IMAGE%.img}-vars.fd"
OVMF_CODE="/usr/share/edk2/x64/OVMF_CODE.4m.fd"

if [[ ! -f "$IMAGE" ]]; then
    echo "[ERROR] Image-Datei '${IMAGE}' nicht gefunden." >&2
    echo "        Zuerst: sudo ./scripts/create-test-vm.sh" >&2
    exit 1
fi

if [[ ! -f "$VARS" ]]; then
    echo "[ERROR] OVMF-Vars '${VARS}' nicht gefunden." >&2
    echo "        Zuerst: sudo ./scripts/create-test-vm.sh" >&2
    exit 1
fi

if [[ ! -f "$OVMF_CODE" ]]; then
    echo "[ERROR] OVMF-Firmware nicht gefunden: ${OVMF_CODE}" >&2
    echo "        Installieren mit: pacman -S edk2-ovmf" >&2
    exit 1
fi

echo "[INFO]  Starte VM: ${IMAGE}"
echo "[INFO]  SSH:       ssh -p 2222 root@localhost"
echo "[INFO]  Beenden:   poweroff (in der VM) oder Ctrl+A X"
echo ""

exec qemu-system-x86_64 \
    -enable-kvm \
    -m 2048 \
    -cpu host \
    -smp 2 \
    -drive "file=${IMAGE},format=raw,if=virtio" \
    -drive "if=pflash,format=raw,readonly=on,file=${OVMF_CODE}" \
    -drive "if=pflash,format=raw,file=${VARS}" \
    -netdev "user,id=net0,hostfwd=tcp::2222-:22" \
    -device "virtio-net-pci,netdev=net0" \
    -nographic
