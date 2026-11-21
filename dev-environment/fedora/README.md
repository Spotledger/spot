# Fedora 44 dev environment

A reproducible Fedora 44 image with everything the repository needs to build and
test: the Rust toolchain from `rust-toolchain.toml`, the .NET SDK from
`clients/avalonia/global.json`, Python 3.12, PostgreSQL 18 client and server,
and the C / cmake / pkg-config build tools in `Cargo.lock`.

| File | What it is |
| --- | --- |
| `Containerfile` | The image. Base pinned by digest; toolchains and hashes from `build.sh`. |
| `pins.env` | The pinned artefacts and their SHA-256/SHA-512. Source of truth for `build.sh`. |
| `build.sh` (`../build.sh`) | Downloads and verifies the artefacts, then builds the image. |
| `verify.sh` | Run inside the image: checks the pinned versions and builds a fast subset. |

## Build and run

```sh
dev-environment/build.sh            # build spotledger-dev:fedora44
dev-environment/build.sh --verify   # build, then run verify.sh in the image
```

The build downloads the rustup installer and the .NET SDK tarball into
`dev-environment/.cache`, verifies each against `pins.env`, and passes the hash
to the Containerfile, which verifies it again. The image build itself makes no
other network request, and dnf verifies every RPM against the Fedora and PGDG
keys. `--refresh` re-downloads if a pin moves.

Run the image with the repository mounted at `/work`:

```sh
podman run --rm -it -v "$PWD:/work:Z" -w /work spotledger-dev:fedora44 bash
podman run --rm -v "$PWD:/work:Z" -w /work -e SPOT_REPO=/work \
  spotledger-dev:fedora44 bash dev-environment/fedora/verify.sh
```

Docker works the same way (`CONTAINER=docker dev-environment/build.sh`).

### toolbox

On a Fedora host, `toolbox` gives the same image with your home directory shared:

```sh
toolbox create --image spotledger-dev:fedora44 spot
toolbox enter spot
cd ~/spot/spot && cargo fmt --all -- --check
```

### distrobox

On any distribution, `distrobox` runs the image with your home mounted and lets
you use it as a terminal; useful when the host has no Fedora packages:

```sh
distrobox create --image spotledger-dev:fedora44 --name spot
distrobox enter spot
cd ~/spot/spot && dev-environment/fedora/verify.sh
```

## PostgreSQL

`tools/ci/start-postgres.sh` starts a throwaway PostgreSQL 18 cluster on a free
port for the backend tests. Fedora's PGDG packages put the binaries in
`/usr/pgsql-18/bin`, which the image sets as `SPOT_PG_BIN`, so the script finds
`initdb` and `pg_ctl` without Docker. Run it from the repository root, or point
the tests at a cluster you start yourself through `SPOT_TEST_PG_URL`.

## What is deliberately absent

The repository uses no protobuf (`Cargo.lock` has no prost, tonic or
prost-build), so `protoc` is not installed. Add it only with a task that adds a
protobuf dependency and pins the version and hash here.
