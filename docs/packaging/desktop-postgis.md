# PostGIS on the desktop edition

The packaging decision, the evidence behind it, and what the installer and
`spot-pg-supervisor` do with it. WP-PRD-02.T7, issue #1734; ADR-087 sections 4.2, 4.3, 4.7,
11.2 and 11.3 (spike S2).

ADR-087 decided on 27 Oct 2026 that **PostGIS is required on every tier, the desktop edition
included**: there is no built-in `point` fallback and no degraded mode. The desktop edition
runs a PostgreSQL that the installer bundles and the supervisor starts, so the bundle must
contain PostGIS and the first start must create the extension — or the install must fail
loudly. That is what this document records, and what `packaging/postgis/` and the supervisor
implement.

**Licensing is decided and not analysed here** (the architecture owner, 27 Oct 2026: PostGIS is GPLv2 and no
analysis is required). The only licence deliverable is [`packaging/postgis/NOTICE.md`](../../packaging/postgis/NOTICE.md):
the notices and source-offer text the installer carries, with versions and upstream URLs.

## 1. The path chosen: (a) the EDB-built PostgreSQL with the matching PostGIS binaries

The brief names two paths: **(a)** ship the EDB-built PostgreSQL and drop the matching PostGIS
Windows bundle into it, or **(b)** build PostgreSQL and PostGIS ourselves, which drags in
GEOS, PROJ and GDAL builds per platform. **Path (a) is chosen.** The measurements below do not
argue against it, and the evidence does:

| Evidence | Source (read 29 Oct 2026) |
| --- | --- |
| PostGIS's Windows installers are built *for* the EDB distribution: "The installers are designed to work with EnterpriseDb PostgreSQL distributions" | <https://postgis.net/windows_downloads/> |
| The bundle tells you to merge it into a PostgreSQL tree: "Copy the files in this folder to your PostgreSQL/%VERSION% folder (structure should mirror what you have)" | the bundle's own `README.txt` |
| A bundle exists for exactly the PostgreSQL major the platform targets (18), so no PostGIS build is needed per release: `postgis-bundle-pg18-3.6.2x64.zip`, 118.3 MiB | <https://download.osgeo.org/postgis/windows/pg18/> |
| The PostgreSQL major is the one the rest of the platform tests against (CI's `tools/ci/start-postgres.sh` starts PostgreSQL 18) and its current minor is 18.6 | <https://www.postgresql.org/support/versioning/> |
| Path (b) would need PostgreSQL (MSVC or MinGW), PostGIS, GEOS, PROJ and GDAL built for every platform we ship, and their build systems differ; the bundle already ships exactly those libraries (its `version.txt`: GEOS 3.14, PROJ 8.2.1, GDAL 3.9.2) | bundle `version.txt` |

What path (a) costs, stated plainly: the desktop bundle is a GPLv2-covered extension in a
process we ship, the PostGIS patch level follows OSGeo's Windows build rather than our release
cadence, and the bundle carries more than the platform needs (pruned — see §3).

## 2. What ships, measured

`packaging/postgis/MANIFEST.toml` is the bill of materials: two pinned archives with SHA-256 and
byte size, and 32 files the installer places under the PostgreSQL prefix (`<prefix>/bin`,
`<prefix>/share`, `<prefix>/lib`). Every hash in it was computed from a real download on
29 Oct 2026; the PostGIS archive's md5sum also matched the upstream `.md5` sidecar, so the
staged bytes are the genuine artifact.

| Archive | Size | Files used |
| --- | --- | --- |
| `postgresql-18.6-1-windows-x64-binaries.zip` (EDB) | 343,808,005 bytes | 4 (postgres.exe, initdb.exe, pg_ctl.exe, libwinpthread-1.dll) |
| `postgis-bundle-pg18-3.6.2x64.zip` (OSGeo) | 124,037,332 bytes | 28 |
| **Declared payload** | **66,549,863 bytes = 63.5 MiB** | 32 files |

The 63.5 MiB breaks down as: PostgreSQL programs 10.2 MiB, the PostGIS extension's SQL scripts
15.3 MiB, its module and DLL closure 32.6 MiB, PROJ's `proj.db` 7.6 MiB, and the two licence
files 27 KiB.

Staging the declared files from the two real archives into one prefix and running
`verify.py --files` produces 32/32 matches, so the merge the installer performs is exactly what
the manifest describes:

```
$ python3 packaging/postgis/verify.py --manifest packaging/postgis/MANIFEST.toml --files stage/pgsql
ok: packaging/postgis/MANIFEST.toml is valid, and stage/pgsql matches
```

### The DLL closure is not a guess

`objdump -p lib/postgis-3.dll` on the real bundle shows its direct imports: `libgeos_c`,
`libproj_8_2`, `libprotobuf-c-1`, `libstdc++-6`, `libwinpthread-1`, `libxml2-2`, plus Windows
system DLLs. Following those transitively gives the 22 DLLs the manifest declares. Shipping the
control file and `postgis-3.dll` alone would pass `PgLayout::validate` and then fail
`CREATE EXTENSION` with "The specified module could not be found".

Two consequences worth naming:

- **GDAL is not shipped.** Nothing in the closure imports it, so the 36 MB `libgdal-35.dll` and
  the 63 MB `share/contrib/postgis-3.6/proj/` grid data are pruned. Consequence: the
  GDAL-backed PostGIS functions and raster I/O are unavailable in the desktop bundle. ADR-087
  requires `geometry(Point,4326)`, GiST and `ST_DWithin`, none of which touch GDAL.
- **PROJ needs `PROJ_LIB`.** PROJ 8.2 locates its resource files through `PROJ_LIB` (its
  `environmentvars` documentation, 8.2.1 tag) and otherwise looks under `share/proj`; the file
  here lives at `share/contrib/postgis-3.6/proj/proj.db`, which is not a default search path.
  The service must therefore pass `PROJ_LIB=<prefix>\share\contrib\postgis-3.6\proj` to the
  child (`SupervisorConfig::child_env` carries it). Coordinate transforms fail without it;
  `ST_DWithin` does not need it.

## 3. Installer size delta

Measured (payload on disk, uncompressed, from the real archives):

- **PostGIS adds 53.3 MiB** to a minimal bundle: 66,549,863 bytes of payload less the
  10,646,528 bytes of the four PostgreSQL files.
- The two archives the build downloads are 446 MiB together; merging and pruning brings the
  installed set to 63.5 MiB, against 289 MiB for the unpacked vendor PostGIS bundle and 871 MiB
  for the whole EDB archive (measured from the archives' file sizes, which include pgAdmin,
  StackBuilder, docs and the pruned extensions).
- The MSI's compressed contribution is **not measured** — there is no MSI yet (WP-PRD-03). The
  number to beat is the installer's own acceptance, not an estimate: see §7.

## 4. `initdb` and `CREATE EXTENSION` timing

Measured on a Linux container (PostgreSQL 18.6, PostGIS 3.6.4), full output and procedure in
[`packaging/postgis/timing.md`](../../packaging/postgis/timing.md):

| Step | Time |
| --- | --- |
| `initdb` (UTF-8, `--no-instructions`) | 3,964 ms |
| `CREATE EXTENSION postgis` on the fresh cluster | 502 / 590 / 576 ms (three runs) |
| Empty database after the extension | 17 MB |

So initialising a cluster and creating the extension costs about **4.6 s** — far inside
WP-PRD-03's five-minute install budget. The Windows/EDB numbers are not measured here and are
not extrapolated; `timing.md` lists the commands that fill them in.

## 5. First start: who creates the extension, and when

ADR-087 section 4.3 puts the desktop case on `spot-pg-supervisor`: "the installer ships the
PostGIS binaries with the bundled PostgreSQL, and `spot-pg-supervisor` calls
`extension.install` at first start, after `initdb`, with the local superuser it owns."

The implementation, in `crates/framework/spot-pg-supervisor/`:

1. `PgLayout::validate()` refuses a bundle without PostGIS **before `initdb` runs**. It checks
   the control file in `<prefix>/share/extension`, reads `module_pathname` out of it
   (`'$libdir/postgis-3'`), and requires that module in `<prefix>/lib`. A user running the
   installer therefore gets "the bundle carries no usable PostGIS … missing
   share/extension/postgis.control" instead of a database that cannot store a `geo` field.
2. `start()` brings the cluster up and waits for the port (the existing readiness probe).
3. **Before `Ready` is published**, the monitor task calls the host's `extension.install`
   adapter once for `postgis` — the `ExtensionProvisioner` trait, implemented by the host over
   `spot-pg-migrate`'s dry run plus the install bound to the dry run's plan hash. The
   supervisor does not depend on `spot-pg-migrate`; it only knows the trait.
4. Only after that call succeeds is `Ready` published, so nothing downstream can observe a
   usable data directory that has no PostGIS.
5. **Later starts are idempotent**: the call is made again, `extension.install` finds the
   extension present at an acceptable version and changes nothing (`already_present`), and the
   supervisor publishes `ExtensionProvisioned { already_present: true }`. A crash restart in
   the same round does not provision again: the extension belongs to the data directory, not to
   the process generation.

### Failure is fatal, and there is no switch

If the module is missing, the offered version is below the contract's minimum
(`default_min_version("postgis") = "3.0"` in `spot-pg-migrate`), the role lacks the privilege
or the call errors, then:

- the state becomes `Failed { reason }` with the typed
  `SupervisorError::PostgisUnprovisionable`, whose message names PostGIS and carries the host's
  typed text (for example `migrate.extension_version: …`);
- an `ExtensionProvisionFailed` supervisor event is published, and an `error!` line is logged
  that carries only the extension name and the reason — no connection string, no password, no
  row value;
- `Ready` is never published, `Handle::ready()` returns the failure, and the child is stopped;
- no configuration value, environment variable or fallback turns this into a warning. There is
  no "degraded" desktop install.

The tests prove each step with the crate's fake bundle and a fake provisioner
(`tests/provisioning.rs`): provision once after readiness and never before; the idempotent
second start; a failing provisioner that never lets `Ready` be published; and no re-provisioning
after a crash restart.

## 6. An existing data directory gains PostGIS without a re-init

`CREATE EXTENSION` is a catalogue operation inside a running database, not a property of
`initdb`. Therefore:

- A data directory created by an earlier build of the bundle (or by an earlier release without
  PostGIS) gains the extension on its **next start**: the supervisor's first-start gate runs on
  every start, `extension.install` plans against the running cluster and installs. No `initdb`,
  no dump and restore, no data loss — `initdb` is never re-run over a directory holding
  `PG_VERSION` (`ensure_initialised` returns `AlreadyInitialised`).
- The extension's own objects are created in `public` by default; no user table is touched.
  Existing `geo` columns cannot exist without it (ADR-087 section 4.7), so there is nothing to
  backfill.
- The same call is what upgrades the extension after a bundle update (`ALTER EXTENSION postgis
  UPDATE`, or `SELECT postgis_extensions_upgrade()` as the bundle's README recommends): the new
  files are on disk, the next start's install finds them and the version requirement is met.

The one case that is refused rather than repaired: a data directory that exists, holds files and
has **no** `PG_VERSION` is half-initialised, and `ensure_initialised` refuses it
(`HalfInitialised`) instead of deleting it — a human decides.

## 7. The Windows upgrade path

Three upgrades are possible, and they are different operations:

1. **PostGIS patch upgrade within the same PostgreSQL major** (for example 3.6.2 → 3.6.x).
   Replace the declared files with the new bundle's (the manifest pins the new version and
   hashes), restart the service, and the extension's own upgrade scripts — the
   `postgis--<old>--<new>.sql` files the bundle ships, of which the manifest carries
   `postgis--ANY--3.6.2.sql` — are applied by `ALTER EXTENSION postgis UPDATE`. Data is
   untouched. The installer's rollback is "put the previous files back": the extension's
   catalogue rows still reference the previous SQL version until the update runs.
2. **PostgreSQL major upgrade** (for example 18 → 19). PostgreSQL's own rule applies: the data
   directory is not compatible across majors, so either a dump/restore or `pg_upgrade` is
   required (<https://www.postgresql.org/docs/current/pgupgrade.html>). `pg_upgrade` needs the
   **old and the new** server binaries at the same time, and each major needs the PostGIS build
   for **that** major — the Windows bundles are per-major (`postgis-bundle-pg18…`,
   `…-pg19…`). The installer must therefore stage the new prefix beside the old one, run
   `pg_upgrade` with `--old-bindir`/`--new-bindir`, then point the service at the new prefix.
   **Not verified here:** whether a `pg_upgrade` of a database with `postgis` installed across
   these majors needs a `postgis_extensions_upgrade()` pass afterwards, and whether the EDB
   binaries of two majors can coexist in one prefix. Both are WP-PRD-03/upgrade work and are
   listed under "Open questions" in the task report.
3. **A repair reinstall of the same version.** Stop the service, replace the files, start it.
   The provisioning call is a no-op; the data directory is untouched.

## 8. What this task delivers, and where

| Piece | Path | What it is |
| --- | --- | --- |
| Bill of materials | `packaging/postgis/MANIFEST.toml` | pinned versions, per-platform file list, URLs, sizes, SHA-256s, the contract's `min_version` |
| Verifier | `packaging/postgis/verify.py` (+ `test_verify.py`) | stdlib-only shape check, and `--files <prefix>` hash check; exit 0 clean, 1 invalid/mismatch, 2 unpinned |
| Notices | `packaging/postgis/NOTICE.md` | PostGIS/GEOS/PROJ/GDAL notices and source offers, with versions and upstream URLs |
| Timing | `packaging/postgis/timing.md` (+ `timing.sh`) | procedure and measured numbers |
| Layout check | `PgLayout::validate` in `spot-pg-supervisor` | refuses a bundle without PostGIS before `initdb` |
| First-start provisioning | `spot-pg-supervisor` (`extensions` module, `Supervisor::with_extension_provisioner`) | calls `extension.install` for `postgis` before `Ready`, fails loudly, idempotent on later starts |
| This document | `docs/packaging/desktop-postgis.md` | the decision, the evidence, the numbers |

No binary is committed to git: the manifest pins what the build downloads, and `verify.py`
checks both the archives and a staged prefix.

## 9. Out of scope

The MSI, WiX and auto-update (WP-PRD-03); the `extension.install` implementation (WP-STO-12.T2,
already delivered); Linux server packaging (distribution packages, not a bundle); the choice of
PostgreSQL major version beyond what the manifest pins; any licence analysis.

## 10. Not verified, and what would settle it

- The **Windows** timings and the installer's five-minute acceptance: no Windows runner here;
  `packaging/postgis/timing.sh` runs on the installed bundle and fills them in.
- The MSI's compressed size contribution.
- `pg_upgrade` across majors with PostGIS installed (see §7).
- Whether a future PostgreSQL minor release changes the DLL closure: `verify.py --files` on a
  staged prefix catches it, and the manifest must be re-pinned when the archive changes.
- `PROJ_LIB` is required at run time (evidence in §2) but no test in this task exercises a
  coordinate transform; a parity case on Windows would.
