#!/usr/bin/env bash
# Build the Fedora 44 Spotledger dev image (WP-PRD-10.T1).
#
#   dev-environment/build.sh            # download the pinned artefacts, build the image
#   dev-environment/build.sh --verify   # ...then run fedora/verify.sh in the image
#   dev-environment/build.sh --refresh  # re-download the artefacts even if cached
#
# It downloads every artefact into dev-environment/.cache, verifies it against
# the hash in fedora/pins.env *and* passes the hash to the Containerfile, which
# verifies it again. Nothing else touches the network during the build: the
# toolchains come from those bytes, and dnf verifies RPMs against the Fedora and
# PGDG keys. The versions come from the repository pins:
#   rust-toolchain.toml          -> Rust toolchain
#   clients/avalonia/global.json -> .NET SDK
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/.." && pwd)"
# shellcheck source=fedora/pins.env
. "$here/fedora/pins.env"

image_tag="${SPOT_DEV_IMAGE:-spotledger-dev:fedora44}"
container="${CONTAINER:-$(command -v podman || command -v docker || true)}"
cache="$here/.cache"
refresh=0
verify=0
for arg in "$@"; do
  case "$arg" in
    --refresh) refresh=1 ;;
    --verify) verify=1 ;;
    -h|--help) sed -n '2,14p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "build.sh: unknown argument $arg" >&2; exit 2 ;;
  esac
done

[ -n "$container" ] || { echo "build.sh: need podman or docker on PATH" >&2; exit 1; }
command -v python3 >/dev/null || { echo "build.sh: need python3 to read the repo pins" >&2; exit 1; }
mkdir -p "$cache"

# The toolchain versions are the repository's, never this script's.
rust_toolchain="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$repo_root/rust-toolchain.toml")"
[ -n "$rust_toolchain" ] || { echo "build.sh: no channel in rust-toolchain.toml" >&2; exit 1; }
dotnet_sdk="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sdk"]["version"])' "$repo_root/clients/avalonia/global.json")"
[ -n "$dotnet_sdk" ] || { echo "build.sh: no sdk version in clients/avalonia/global.json" >&2; exit 1; }
dotnet_url="$DOTNET_SDK_BASE/Sdk/$dotnet_sdk/dotnet-sdk-$dotnet_sdk-linux-x64.tar.gz"

fetch() { # <url> <destination> <algorithm> <expected-hash>
  local url="$1" dest="$2" algorithm="$3" expected="$4"
  if [ "$refresh" = 1 ] || [ ! -f "$dest" ]; then
    echo "build.sh: downloading $url"
    curl -fSL --retry 3 --output "$dest.part" "$url"
    mv "$dest.part" "$dest"
  fi
  local actual
  actual="$("${algorithm}sum" "$dest" | cut -d' ' -f1)"
  if [ "$actual" != "$expected" ]; then
    echo "build.sh: ${algorithm} mismatch for $dest" >&2
    echo "  expected $expected" >&2
    echo "  actual   $actual" >&2
    exit 1
  fi
  echo "build.sh: verified $dest (${algorithm})"
}

fetch "$RUSTUP_INIT_URL" "$cache/rustup-init" sha256 "$RUSTUP_INIT_SHA256"
fetch "$dotnet_url" "$cache/dotnet-sdk.tar.gz" sha512 "$DOTNET_SDK_SHA512"

echo "build.sh: building $image_tag ($container) with Rust $rust_toolchain and .NET SDK $dotnet_sdk"
"$container" build \
  --file "$here/fedora/Containerfile" \
  --tag "$image_tag" \
  --build-arg "FEDORA_IMAGE=$FEDORA_IMAGE" \
  --build-arg "RUST_TOOLCHAIN=$rust_toolchain" \
  --build-arg "DOTNET_SDK=$dotnet_sdk" \
  --build-arg "RUSTUP_INIT_SHA256=$RUSTUP_INIT_SHA256" \
  --build-arg "DOTNET_SDK_SHA512=$DOTNET_SDK_SHA512" \
  "$here"

if [ "$verify" = 1 ]; then
  echo "build.sh: running fedora/verify.sh in the image"
  "$container" run --rm \
    --volume "$repo_root:/work:Z" \
    --workdir /work \
    --env SPOT_REPO=/work \
    "$image_tag" bash dev-environment/fedora/verify.sh
fi

echo "build.sh: done. Run it with:"
echo "  $container run --rm -it --volume \"$repo_root:/work:Z\" --workdir /work $image_tag bash"
