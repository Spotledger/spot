Part of [Running Spot](../running-spot.md).

# File providers: local, S3-compatible, Azure Blob

`[services.files] provider` chooses where a site's uploaded files live. `local` (the default) and
`memory` (tests) are always available. `s3` and `azure` store the files in a bucket or container
and are compiled in only when the binary is built with the `files-s3` or `files-azure` cargo
feature. Both are **off by default**, so the default build pulls no cloud client.

An unknown provider string is an error (it used to mean `local` silently). A configuration that
cannot work stops the site at start-up with a message that names the key and never a secret.

## Choosing

| Provider | Use it when | Files live |
| --- | --- | --- |
| `local` | one node, a disk you back up | `sites/<site>/<root>` on the node |
| `s3` | several nodes, or the files must outlive the node: AWS S3, Cloudflare R2, MinIO, Wasabi | a bucket |
| `azure` | the same on Azure | a blob container |

## Settings

Credentials are references, never values: `env:NAME` (an upper-case variable of the process) or
`file:/absolute/path` (a file the process owner alone can read, mode 0600). They are never inline
because a `common.toml` is copied, committed and pasted into tickets.

An S3 bucket on AWS, credentials from the process environment (the AWS default chain: instance
role, `AWS_ACCESS_KEY_ID`, ...):

```toml
[services.files]
provider = "s3"
[services.files.s3]
bucket = "acme-spot-files"
region = "eu-west-1"
```

Cloudflare R2 (an endpoint, static credentials by reference):

```toml
[services.files]
provider = "s3"
[services.files.s3]
bucket = "acme-spot-files"
endpoint = "https://<account>.r2.cloudflarestorage.com"
region = "auto"
access_key_id_ref = "env:R2_ACCESS_KEY_ID"
secret_access_key_ref = "file:/etc/spot/r2-secret"
```

MinIO on this machine (plain HTTP is allowed to loopback only):

```toml
[services.files]
provider = "s3"
[services.files.s3]
bucket = "spot-files"
endpoint = "http://127.0.0.1:9000"
allow_http = true
path_style = true
access_key_id_ref = "env:MINIO_ROOT_USER"
secret_access_key_ref = "env:MINIO_ROOT_PASSWORD"
```

Azure Blob:

```toml
[services.files]
provider = "azure"
[services.files.azure]
account = "acmefiles"
container = "spot-files"
credential_kind = "access_key"      # or "sas"
credential_ref = "file:/etc/spot/azure-key"
```

Rules, all checked at start-up:

- a sub-table for a provider other than the selected one is refused;
- `bucket` (S3) or `account` and `container` (Azure) are required; with S3, either both
  `access_key_id_ref` and `secret_access_key_ref` or neither;
- `allow_http` on a non-loopback endpoint is refused unless the table also sets
  `allow_insecure_remote = true`; readiness then says `(insecure)`. Never for production;
- TLS is verified.

## The bucket

The bucket or container **must already exist**: the service never creates one. The credential needs
`put`, `get`, `delete` and `list` on the site's prefix. A bucket policy should deny public access
and everything else outside that prefix. Keys are stored as `<site>/<key>`, so one bucket can serve
several sites; there is no setting to choose a prefix.

## Documented compromises of the object store

`get` reads the whole object into memory; each upload passes through a local spool file while it
is hashed; `list` walks every directory level and pages in memory; a conditional create of a body
over the single-put limit is not one atomic request. Callers must not assume what `local`
guarantees unconditionally.

## Readiness

At start-up the files service writes 16 bytes under `private/probe-<random>` in the site's own
prefix, reads them back, compares and deletes them, each step with its own message (`cannot write`,
`cannot read back`, `cannot delete`, `bucket not reachable`) and a 10 second limit (`timeout`). The
message names `s3 bucket <name>` or `azure container <name>`, never a key, an endpoint query or a
credential. The service is `Required` in the cluster profile, optional otherwise.

## Troubleshooting

| Symptom | Cause | What to do |
| --- | --- | --- |
| `built without the files-s3 feature` at start | The binary has no S3 provider | Build with `--features files-s3` (or `files-azure`), or use another provider |
| `services.files.s3.secret_access_key_ref: ... not set or is empty` / `cannot be read` | The reference names a missing variable or an unreadable file | Set the variable, or fix the path and make the file mode 0600 |
| Probe says `cannot write` | No `put` permission on the prefix, a bucket that does not exist, or an endpoint that does not answer | Create the bucket; check the policy and the endpoint |
| 503 `server.unavailable` on upload during an outage | The object store is unreachable; the provider answers `Unavailable` | Retry; the upload is not half-stored |
| Probe or uploads fail with a region error | Wrong `region` for the bucket (R2 uses `auto`) | Set the bucket's region |
| `RequestTimeTooSkewed` or a signature error | The node's clock is minutes off | Fix the clock (NTP); the object store rejects skewed requests |
| `allow_http on a remote endpoint is refused` | Plain HTTP to a non-loopback host | Use `https`, or for a lab only `allow_insecure_remote = true` |
