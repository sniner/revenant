# tools/lib-vm.sh — shared constants and helpers for revenant VM scripts.
#
# Source this from bash scripts only:
#   . "$(dirname "$0")/lib-vm.sh"
#
# Exposes:
#   constants:    OVMF_CODE, OVMF_VARS_SRC,
#                 REVENANT_VM_IMAGE_DEFAULT, REVENANT_VM_SSH_PORT,
#                 REVENANT_VM_SSH_USER, REVENANT_VM_SSH_PASS
#   derivations:  vm_vars_file <image>
#   checks:       require_tools <tool…>, require_ovmf_code, require_ovmf_vars_src
#   logging:      info, step, pass, fail, die
#
# `fail` here is a plain printer. Scripts that need to track failure state
# (e.g. run-vm-tests.sh) may redefine it after sourcing.

# Guard against double-sourcing.
if [[ -n "${__REVENANT_LIB_VM:-}" ]]; then return 0; fi
__REVENANT_LIB_VM=1

# ── Constants ─────────────────────────────────────────────────────────────────

REVENANT_VM_IMAGE_DEFAULT="revenant-test.img"
REVENANT_VM_SSH_PORT=2222
REVENANT_VM_SSH_USER="root"
REVENANT_VM_SSH_PASS="revenant"
OVMF_CODE="/usr/share/edk2/x64/OVMF_CODE.4m.fd"
OVMF_VARS_SRC="/usr/share/edk2/x64/OVMF_VARS.4m.fd"

# ── Colors (TTY-aware) ────────────────────────────────────────────────────────

if [[ -t 1 ]]; then
    __C_RED=$'\033[31m'; __C_GREEN=$'\033[32m'; __C_YELLOW=$'\033[33m'
    __C_BLUE=$'\033[34m'; __C_OFF=$'\033[0m'
else
    __C_RED=""; __C_GREEN=""; __C_YELLOW=""; __C_BLUE=""; __C_OFF=""
fi

# ── Logging ───────────────────────────────────────────────────────────────────

info() { echo "${__C_BLUE}[info]${__C_OFF}  $*"; }
step() { echo "${__C_YELLOW}[step]${__C_OFF}  $*"; }
pass() { echo "${__C_GREEN}[PASS]${__C_OFF}  $*"; }
fail() { echo "${__C_RED}[FAIL]${__C_OFF}  $*"; }

die() {
    local msg="$1"
    local code="${2:-2}"
    echo "${__C_RED}[fatal]${__C_OFF} $msg" >&2
    exit "$code"
}

# ── Derivations ───────────────────────────────────────────────────────────────

vm_vars_file() {
    echo "${1%.img}-vars.fd"
}

# ── Precondition checks ───────────────────────────────────────────────────────

require_tools() {
    local missing=()
    local t
    for t in "$@"; do
        command -v "$t" >/dev/null 2>&1 || missing+=("$t")
    done
    if (( ${#missing[@]} > 0 )); then
        echo "${__C_RED}[fatal]${__C_OFF} missing tools:" >&2
        for t in "${missing[@]}"; do echo "  - $t" >&2; done
        exit 2
    fi
}

require_ovmf_code() {
    [[ -f "$OVMF_CODE" ]] \
        || die "OVMF firmware not found: $OVMF_CODE (install: pacman -S edk2-ovmf)"
}

require_ovmf_vars_src() {
    [[ -f "$OVMF_VARS_SRC" ]] \
        || die "OVMF vars template not found: $OVMF_VARS_SRC (install: pacman -S edk2-ovmf)"
}
