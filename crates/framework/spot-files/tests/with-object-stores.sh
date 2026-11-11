#!/usr/bin/env bash
# Runs the given command with the object providers' local emulators running: MinIO
# for the S3-compatible store (feature `s3`), Azurite for the Azure blob store
# (feature `azure`). Where a container engine or one of its images is unavailable,
# each provider falls back to the same emulator without containers: Moto's S3 on
# Python (the same wire API: conditional creates, multipart, user metadata), and
# Azurite through npm. The script only fails when a provider is served by neither.
#
# The script waits for each service to answer, creates the `spot-tests` bucket and
# container, exports the `SPOT_TEST_*` variables the provider tests read
# (SPOT_TEST_S3_{ENDPOINT,BUCKET,KEY,SECRET} and SPOT_TEST_AZURE_{ENDPOINT,ACCOUNT,
# KEY,CONTAINER}), runs the command, and removes the emulators again on any exit,
# including failure:
#
#   bash tests/with-object-stores.sh cargo test -p spot-files --all-features
#
# When the SPOT_TEST_* variables are already set the command runs against those as-is,
# so a run can be pointed at real services or already-running emulators instead.
#
# The container path wants docker or podman on a Linux host (the emulators join the
# host network, so the loopback ports 9001 MinIO, 5000 Moto and 10000 Azurite must be
# free; occupied ports are stepped around on the fallback paths). The non-container
# fallbacks want python3 with pip/venv (Moto) and node with npx (Azurite), fetching
# the same pinned versions as the images.

set -euo pipefail

required=(
  SPOT_TEST_S3_ENDPOINT SPOT_TEST_S3_BUCKET SPOT_TEST_S3_KEY SPOT_TEST_S3_SECRET
  SPOT_TEST_AZURE_ENDPOINT SPOT_TEST_AZURE_ACCOUNT SPOT_TEST_AZURE_KEY SPOT_TEST_AZURE_CONTAINER
)
configured=true
for variable in "${required[@]}"; do
  [ -n "${!variable:-}" ] || configured=false
done
if $configured; then exec "$@"; fi

# One engine: docker, falling back to podman; the emulators run the same on both.
# No engine is survivable: each provider below has a non-container fallback.
engine=
if docker version > /dev/null 2>&1; then engine=docker
elif podman --version > /dev/null 2>&1; then engine=podman
else
  echo "with-object-stores.sh: neither a running docker nor podman is available; using the non-container emulators" >&2
fi

# Pinned emulator versions: the versions the suite's results are known good on. The
# MinIO releases exist as images on Docker Hub (verified against minio/minio and
# minio/mc release tags).
s3_image=minio/minio:RELEASE.2025-09-07T16-13-09Z
mc_image=minio/mc:RELEASE.2025-08-13T08-35-41Z
azure_image=mcr.microsoft.com/azure-storage/azurite:3.33.0
# Moto's S3: the S3 fallback when the MinIO image is not pullable or no engine runs;
# it serves the same wire API (conditional creates, multipart, user metadata). Pinned
# the same for the image and the pip package; moto keeps the bare `x-amz-` user
# metadata on PUT and answers If-None-Match, exactly what the contract suite exercises.
moto_image=motoserver/moto:5.2.3
moto_version=5.2.3
# Azurite through npm: the Azure fallback when no engine runs — the same emulator
# build the image ships, pinned to the image's version. Moto is deliberately NOT an
# Azure fallback: its server speaks S3 only.
azurite_version=3.33.0

bucket_name=spot-tests
s3_user=minioadmin
s3_password=minioadmin

# Azurite's well-known development account: acceptable against the emulator only,
# which is exactly what this script runs.
azure_account=devstoreaccount1
azure_key=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==

s3_port=9001
azure_port=10000

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/spot-object-stores.XXXXXX")
s3_id=
azure_id=
s3_pid=
azure_pid=
cleanup() {
  if [ -n "$engine" ]; then
    [ -n "$s3_id" ] && "$engine" rm -f "$s3_id" > /dev/null 2>&1 || true
    [ -n "$azure_id" ] && "$engine" rm -f "$azure_id" > /dev/null 2>&1 || true
  fi
  if [ -n "$s3_pid" ]; then
    pkill -P "$s3_pid" > /dev/null 2>&1 || true
    kill "$s3_pid" > /dev/null 2>&1 || true
  fi
  if [ -n "$azure_pid" ]; then
    pkill -P "$azure_pid" > /dev/null 2>&1 || true
    kill "$azure_pid" > /dev/null 2>&1 || true
  fi
  rm -rf "$work_dir"
}
trap cleanup EXIT

# wait_for: loops until the URL answers at all (a connection is accepted), 60 seconds.
# Returns 1 for the caller to decide: an attempt that starts a service but cannot make
# it answer is a failed attempt, not the end of the script — the next fallback is fine.
wait_for() {
  local url=$1 name=$2 attempt code
  for attempt in $(seq 1 120); do
    code=$(curl -s -o /dev/null --max-time 2 -w '%{http_code}' "$url" || true)
    [ "$code" != 000 ] && return 0
    sleep 0.5
  done
  echo "with-object-stores.sh: $name did not become reachable at $url in time" >&2
  return 1
}

# free_port: echoes the first TCP port on 127.0.0.1 from $1 upwards nobody is
# listening on (a fallback server cannot assume its default port is free).
free_port() {
  local port=$1
  while [ "$port" -lt $(($1 + 50)) ]; do
    if ! (exec 3<> "/dev/tcp/127.0.0.1/$port") 2> /dev/null; then
      echo "$port"
      return 0
    fi
    port=$((port + 1))
  done
  return 1
}

# --------------------------------------------------------------------------- S3 side

# Three ways to speak S3, tried in order: MinIO (container), Moto (container when an
# engine exists but MinIO's image is not pullable), Moto (Python, engine-free).

start_s3_minio() {
  [ -n "$engine" ] || return 1
  "$engine" pull "$s3_image" > /dev/null 2>&1 || return 1
  echo "with-object-stores.sh: starting MinIO ($s3_image) on :$s3_port" >&2
  s3_id=$("$engine" run --rm -d --network host "$s3_image" server /data --address ":$s3_port") || return 1
  if ! wait_for "http://127.0.0.1:$s3_port/minio/health/ready" "MinIO"; then
    "$engine" rm -f "$s3_id" > /dev/null 2>&1
    s3_id=
    return 1
  fi
  # The bucket, via a throwaway mc container talking to the running one.
  if ! "$engine" run --rm --network host --entrypoint /bin/sh "$mc_image" -c \
    "mc alias set local http://127.0.0.1:$s3_port $s3_user $s3_password > /dev/null &&
     mc mb --ignore-existing local/$bucket_name" > /dev/null 2>&1; then
    "$engine" rm -f "$s3_id" > /dev/null 2>&1
    s3_id=
    return 1
  fi
}

start_s3_moto_container() {
  [ -n "$engine" ] || return 1
  "$engine" pull "$moto_image" > /dev/null 2>&1 || return 1
  echo "with-object-stores.sh: MinIO unavailable; falling back to Moto's S3 container ($moto_image)" >&2
  s3_port=$(free_port 5000) || return 1
  s3_id=$("$engine" run --rm -d --network host "$moto_image" "$s3_port") || return 1
  if ! wait_for "http://127.0.0.1:$s3_port" "Moto"; then
    "$engine" rm -f "$s3_id" > /dev/null 2>&1
    s3_id=
    return 1
  fi
  s3_user=test
  s3_password=test
  # Moto creates the bucket on a plain unauthenticated PUT.
  curl -s -o /dev/null -X PUT "http://127.0.0.1:$s3_port/$bucket_name"
}

start_s3_moto_python() {
  # Moto's S3 as a Python process: a throwaway venv with the pinned package, or
  # `uvx` when the caller names it (uv resolves and caches the pin).
  echo "with-object-stores.sh: no container path served S3; falling back to Moto's S3 on Python (moto[server]==$moto_version)" >&2
  s3_port=$(free_port 5000) || return 1
  if [ "${1:-venv}" = uvx ]; then
    uvx --from "moto[server]==$moto_version" --exe moto_server -p "$s3_port" > /dev/null 2>&1 &
    s3_pid=$!
  else
    python3 -m venv "$work_dir/moto-venv" > /dev/null 2>&1 || return 1
    "$work_dir/moto-venv/bin/pip" install --quiet --disable-pip-version-check \
      "moto[server]==$moto_version" > /dev/null 2>&1 || return 1
    "$work_dir/moto-venv/bin/moto_server" -p "$s3_port" > /dev/null 2>&1 &
    s3_pid=$!
  fi
  if ! wait_for "http://127.0.0.1:$s3_port" "Moto"; then
    kill "$s3_pid" > /dev/null 2>&1 || true
    s3_pid=
    return 1
  fi
  s3_user=test
  s3_password=test
  curl -s -o /dev/null -X PUT "http://127.0.0.1:$s3_port/$bucket_name"
}

s3_started=false
if start_s3_minio; then
  s3_started=true
elif start_s3_moto_container; then
  s3_started=true
elif command -v python3 > /dev/null 2>&1 && start_s3_moto_python venv; then
  s3_started=true
elif command -v uvx > /dev/null 2>&1 && start_s3_moto_python uvx; then
  s3_started=true
fi
if ! $s3_started; then
  echo "with-object-stores.sh: no S3 emulator could be started (MinIO image, Moto image, or Moto via python3/uvx) — the acceptance runner needs docker/podman with pullable images, or a python3 whose pip can install moto[server]==$moto_version" >&2
  exit 1
fi

# ----------------------------------------------------------------------- Azure side

# Two ways to speak the Azure blob API, tried in order: the Azurite container and the
# same Azurite through npx. They serve the identical endpoint, account and key, so the
# signed container creation below works against either.

start_azure_container() {
  [ -n "$engine" ] || return 1
  echo "with-object-stores.sh: starting Azurite ($azure_image) on :$azure_port" >&2
  azure_id=$("$engine" run --rm -d --network host "$azure_image" \
    azurite-blob --blobHost 127.0.0.1 --blobPort "$azure_port") || return 1
  if ! wait_for "http://127.0.0.1:$azure_port/$azure_account" "Azurite"; then
    "$engine" rm -f "$azure_id" > /dev/null 2>&1
    azure_id=
    return 1
  fi
}

start_azure_npx() {
  command -v npx > /dev/null 2>&1 || return 1
  echo "with-object-stores.sh: no container path served Azure; falling back to Azurite through npx (azurite@$azurite_version)" >&2
  azure_port=$(free_port 10000) || return 1
  npx --yes "azurite@$azurite_version" blob \
    --location "$work_dir/azurite" --blobHost 127.0.0.1 --blobPort "$azure_port" \
    > /dev/null 2>&1 &
  azure_pid=$!
  if ! wait_for "http://127.0.0.1:$azure_port/$azure_account" "Azurite"; then
    pkill -P "$azure_pid" > /dev/null 2>&1 || true
    kill "$azure_pid" > /dev/null 2>&1 || true
    azure_pid=
    return 1
  fi
}

start_azure_nodeenv() {
  # Last Azure resort on an engine-free runner without nodejs on PATH: the
  # guaranteed python fetches a pinned prebuilt Node through `nodeenv` (pip) and
  # Azurite through that node's npm. Same pins as everywhere else.
  command -v python3 > /dev/null 2>&1 || return 1
  echo "with-object-stores.sh: no npx; falling back to Node+Azurite through python3 nodeenv (node@22.14.0, azurite@$azurite_version)" >&2
  azure_port=$(free_port 10000) || return 1
  python3 -m venv "$work_dir/nodeenv" > /dev/null 2>&1 || return 1
  "$work_dir/nodeenv/bin/pip" install --quiet --disable-pip-version-check \
    nodeenv==1.9.1 > /dev/null 2>&1 || return 1
  "$work_dir/nodeenv/bin/nodeenv" --prebuilt --node=22.14.0 "$work_dir/node" \
    > /dev/null 2>&1 || return 1
  (cd "$work_dir" && PATH="$work_dir/node/bin:$PATH" npm install --silent \
    --no-audit --no-fund "azurite@$azurite_version") > /dev/null 2>&1 || return 1
  "$work_dir/node/bin/node" "$work_dir/node_modules/.bin/azurite" blob \
    --location "$work_dir/azurite" --blobHost 127.0.0.1 --blobPort "$azure_port" \
    > /dev/null 2>&1 &
  azure_pid=$!
  if ! wait_for "http://127.0.0.1:$azure_port/$azure_account" "Azurite"; then
    pkill -P "$azure_pid" > /dev/null 2>&1 || true
    kill "$azure_pid" > /dev/null 2>&1 || true
    azure_pid=
    return 1
  fi
}

azure_started=false
if start_azure_container; then
  azure_started=true
elif start_azure_npx; then
  azure_started=true
elif start_azure_nodeenv; then
  azure_started=true
fi
if ! $azure_started; then
  echo "with-object-stores.sh: no Azure emulator could be started (Azurite image, Azurite via npx, or Node+Azurite via python3 nodeenv) — the acceptance runner needs docker/podman with pullable images, or node with npx, or a python3 whose pip can install nodeenv with access to nodejs.org and the npm registry" >&2
  exit 1
fi

# The container, with a signed Put Container request against the blob REST API
# (`PUT /container?restype=container`; query parameters belong to the canonical
# resource). The SharedKey signature: HMAC-SHA256 over the canonical request with the
# base64-decoded account key. Eleven empty header slots stand for Content-Encoding,
# Content-Language, Content-Length, Content-MD5, Content-Type, Date, If-Modified-Since,
# If-Match, If-None-Match, If-Unmodified-Since, Range (the x-ms-* headers are
# canonicalized separately), and the twelfth newline separates them from the
# canonicalized headers, so the string carries twelve newlines ahead of `x-ms-date`.
# Note the emulator resolves account-style paths itself, so its canonicalized
# resource carries the account name twice.
stamp=$(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S GMT')
xms_version=2023-11-03
string_to_sign=$'PUT\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:'"$stamp"$'\nx-ms-version:'"$xms_version"$'\n/'"$azure_account"/"$azure_account"/"$bucket_name"$'\nrestype:container'
hex_key=$(printf %s "$azure_key" | base64 -d | od -An -v -tx1 | tr -d ' \n')
signature=$(printf %s "$string_to_sign" | openssl dgst -sha256 -mac HMAC -macopt "hexkey:$hex_key" -binary | base64)
if ! curl -sf -o /dev/null --max-time 10 -X PUT \
  -H "Content-Length: 0" -H "x-ms-date: $stamp" -H "x-ms-version: $xms_version" \
  -H "Authorization: SharedKey $azure_account:$signature" \
  "http://127.0.0.1:$azure_port/$azure_account/$bucket_name?restype=container"; then
  echo "with-object-stores.sh: could not create the Azure container $bucket_name" >&2
  exit 1
fi

export SPOT_TEST_S3_ENDPOINT="http://127.0.0.1:$s3_port"
export SPOT_TEST_S3_BUCKET="$bucket_name"
export SPOT_TEST_S3_KEY="$s3_user"
export SPOT_TEST_S3_SECRET="$s3_password"
export SPOT_TEST_AZURE_ENDPOINT="http://127.0.0.1:$azure_port"
export SPOT_TEST_AZURE_ACCOUNT="$azure_account"
export SPOT_TEST_AZURE_KEY="$azure_key"
export SPOT_TEST_AZURE_CONTAINER="$bucket_name"
# An http endpoint switches object_store to the Azure emulator mode, whose service
# URL comes from this environment (object_store's own default is the same port).
export AZURITE_BLOB_STORAGE_URL="$SPOT_TEST_AZURE_ENDPOINT"

echo "with-object-stores.sh: the S3 and Azure emulators are up; running the command" >&2
"$@"
