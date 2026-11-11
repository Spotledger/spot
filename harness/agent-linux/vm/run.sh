#!/usr/bin/env bash
# VM conformance run (WP-ADM-04.T3): the modules a container cannot exercise.
#
# A container shares the host's LSM (SELinux/AppArmor) and firewall, so the `vm_only`
# tests — os.mac.status, os.firewall.status/list — must run on a genuine VM. This script
# boots a cloud image under qemu, copies the built spot-agent-linux-real test binary in
# over ssh, runs the selected tests, prints the output and tears the VM down. It reads the
# image URL and the vm target list from ../matrix.toml, so the matrix stays the single
# source of truth.
#
# This is NOT executed by CI by default. It needs qemu, cloud-localds (cloud-utils) and
# ssh, and it downloads a multi-hundred-megabyte image. Run it deliberately:
#
#   SPOT_HARNESS_DISPOSABLE=1 ./vm/run.sh fedora-44-vm
#   SPOT_HARNESS_DISPOSABLE=1 ./vm/run.sh --all
#
# The VM is disposable: its disk is a qcow2 overlay under $SPOT_VM_WORKDIR (default
# ${XDG_CACHE_HOME:-$HOME/.cache}/spot-agent-linux-vm) that is deleted at the end.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HARNESS_DIR="$(cd "$HERE/.." && pwd)"
MATRIX="$HARNESS_DIR/matrix.toml"
REAL_CRATE="$HARNESS_DIR/tests-real"

PYTHON="${PYTHON:-python3}"
WORKDIR="${SPOT_VM_WORKDIR:-${XDG_CACHE_HOME:-$HOME/.cache}/spot-agent-linux-vm}"
SSH_PORT="${SPOT_VM_SSH_PORT:-2222}"
SSH_USER="${SPOT_VM_USER:-spot}"
SSH_KEY="${SPOT_VM_SSH_KEY:-$WORKDIR/id_ed25519}"
TIMEOUT_BOOT="${SPOT_VM_BOOT_TIMEOUT:-600}"

log() { echo "[vm] $*" >&2; }
die() { echo "[vm] error: $*" >&2; exit 1; }

require() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"
}

guard() {
  [ "${SPOT_HARNESS_DISPOSABLE:-}" = "1" ] \
    || die "refusing to run: set SPOT_HARNESS_DISPOSABLE=1 (this boots and destroys a VM)"
  if [ ! -e /.dockerenv ] && [ ! -e /run/.containerenv ]; then
    local virt
    virt="$(systemd-detect-virt 2>/dev/null || true)"
    [ -n "$virt" ] && [ "$virt" != "none" ] \
      || die "refusing to run on bare metal: run this inside a container or a VM"
  fi
}

# Reads a field of a target from matrix.toml using the harness's own loader.
matrix_field() {
  local target="$1" field="$2"
  "$PYTHON" - "$HARNESS_DIR" "$MATRIX" "$target" "$field" <<'PY'
import sys
sys.path.insert(0, sys.argv[1])
from agent_linux_harness.matrix import load_matrix

matrix = load_matrix(sys.argv[2])
print(getattr(matrix.target(sys.argv[3]), sys.argv[4]))
PY
}

# The vm targets, in matrix order.
vm_targets() {
  "$PYTHON" - "$HARNESS_DIR" "$MATRIX" <<'PY'
import sys
sys.path.insert(0, sys.argv[1])
from agent_linux_harness.matrix import load_matrix

for target in load_matrix(sys.argv[2]).targets:
    if target.mode == "vm":
        print(target.name)
PY
}

build_binary() {
  log "building the real test binary"
  cargo test --no-run --message-format=json --manifest-path "$REAL_CRATE/Cargo.toml" \
    | "$PYTHON" -c '
import json, sys
exes = []
for line in sys.stdin:
    line = line.strip()
    if not line.startswith("{"):
        continue
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    if msg.get("reason") == "compiler-artifact" and msg.get("executable") \
            and "test" in msg.get("target", {}).get("kind", []):
        exes.append(msg["executable"])
print(exes[-1] if exes else "")
'
}

# Generates the cloud-init user-data and a seed ISO for the VM.
make_seed() {
  local seed_iso="$1"
  [ -f "$SSH_KEY" ] || ssh-keygen -t ed25519 -N "" -f "$SSH_KEY" >/dev/null
  local pubkey
  pubkey="$(cat "$SSH_KEY.pub")"
  cat >"$WORKDIR/user-data" <<EOF
#cloud-config
users:
  - name: $SSH_USER
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    ssh_authorized_keys:
      - $pubkey
ssh_pwauth: false
package_update: false
EOF
  : >"$WORKDIR/meta-data"
  cloud-localds "$seed_iso" "$WORKDIR/user-data" "$WORKDIR/meta-data"
}

wait_for_ssh() {
  local waited=0
  while true; do
    if ssh -i "$SSH_KEY" -p "$SSH_PORT" -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 \
        "$SSH_USER@127.0.0.1" true >/dev/null 2>&1; then
      return 0
    fi
    if [ "$waited" -ge "$TIMEOUT_BOOT" ]; then
      return 1
    fi
    sleep 5
    waited=$((waited + 5))
  done
}

run_target() {
  local target="$1" binary="$2"
  local image_url base overlay seed_iso
  image_url="$(matrix_field "$target" image)"
  base="$WORKDIR/$(basename "$image_url")"
  overlay="$WORKDIR/$target.qcow2"
  seed_iso="$WORKDIR/$target-seed.iso"

  [ -f "$base" ] || { log "downloading $image_url"; curl -fL --retry 3 -o "$base.part" "$image_url"; mv "$base.part" "$base"; }
  rm -f "$overlay"
  qemu-img create -f qcow2 -F qcow2 -b "$base" "$overlay" >/dev/null
  make_seed "$seed_iso"

  local accel="tcg" cpu="max"
  if [ -w /dev/kvm ]; then
    accel="kvm"
    cpu="host"
  fi
  log "booting $target (accel=$accel, ssh port $SSH_PORT)"
  qemu-system-x86_64 \
    -machine "accel=$accel" -cpu "$cpu" -smp 2 -m 4096 \
    -drive "file=$overlay,if=virtio" \
    -drive "file=$seed_iso,format=raw,if=virtio" \
    -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:$SSH_PORT-:22" \
    -device virtio-net-pci,netdev=n0 \
    -nographic -serial "file:$WORKDIR/$target-console.log" \
    -pidfile "$WORKDIR/$target.pid" -daemonize

  if ! wait_for_ssh; then
    log "ssh did not come up; last console lines:"
    tail -n 40 "$WORKDIR/$target-console.log" >&2 || true
    return 1
  fi

  log "copying the test binary and running the vm-only tests"
  scp -i "$SSH_KEY" -P "$SSH_PORT" -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null "$binary" "$SSH_USER@127.0.0.1:/tmp/real-tests"
  ssh -i "$SSH_KEY" -p "$SSH_PORT" -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null "$SSH_USER@127.0.0.1" \
    "chmod +x /tmp/real-tests && sudo env SPOT_HARNESS_DISPOSABLE=1 /tmp/real-tests --ignored --test-threads=1"

  ssh -i "$SSH_KEY" -p "$SSH_PORT" -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null "$SSH_USER@127.0.0.1" "sudo poweroff" || true
}

cleanup() {
  for pidfile in "$WORKDIR"/*.pid; do
    [ -f "$pidfile" ] || continue
    kill "$(cat "$pidfile")" 2>/dev/null || true
  done
}
trap cleanup EXIT

main() {
  guard
  require qemu-system-x86_64
  require qemu-img
  require cloud-localds
  require ssh
  require scp
  require curl
  mkdir -p "$WORKDIR"

  local selected=()
  if [ "$#" -eq 0 ] || [ "$1" = "--all" ]; then
    while IFS= read -r name; do selected+=("$name"); done < <(vm_targets)
  else
    selected=("$@")
  fi
  [ "${#selected[@]}" -gt 0 ] || die "no vm targets selected"

  local binary
  binary="$(build_binary)"
  [ -n "$binary" ] || die "could not build the real test binary"

  local failed=0
  for target in "${selected[@]}"; do
    if ! run_target "$target" "$binary"; then
      log "$target FAILED"
      failed=1
    fi
  done
  exit "$failed"
}

main "$@"
