# Notices the desktop installer must carry

This file is the notice and source-offer text for the PostgreSQL and PostGIS binaries the
Windows desktop installer ships with the bundled database (WP-PRD-02.T7, ADR-087 sections 4.2
and 11.2). The pinned versions, files and hashes are in [`MANIFEST.toml`](MANIFEST.toml); the
packaging decision is in [`docs/packaging/desktop-postgis.md`](../../docs/packaging/desktop-postgis.md).

Per the architecture owner's decision of 27 Oct 2026 there is **no licence analysis** here: PostGIS is GPLv2 and
that is accepted. What follows is what the installer must display or install alongside the
binaries, taken from the upstream distributions and their published sources.

The installer must:

1. place this file at `<prefix>/share/doc/spot/THIRD-PARTY-NOTICES.md` next to the installed
   product, and
2. keep the licence files the manifest declares
   (`share/doc/postgis/COPYING`, `share/doc/postgis/LICENSE`) unmodified beside it, and
3. show the "About → Third-party notices" entry in the product UI, which renders this file.

The archive names, versions and URLs below were read on 29 Oct 2026. The upstream archives are
unmodified OSGeo and EDB builds; the SHA-256 of each download is recorded in `MANIFEST.toml`.

---

## PostGIS 3.6.2

- Upstream: <https://postgis.net/>
- Distribution: `postgis-bundle-pg18-3.6.2x64.zip` from
  <https://download.osgeo.org/postgis/windows/pg18/> (the bundle's own `version.txt` records
  `POSTGIS: 3.6.2`), built for the EDB PostgreSQL 18 for Windows.
- Licence: GNU General Public License, version 2. The bundle carries the full text as
  `bin/COPYING` (installed as `share/doc/postgis/COPYING`); it is the GPL v2 text beginning:

  ```
                      GNU GENERAL PUBLIC LICENSE
                         Version 2, June 1991

   Copyright (C) 1989, 1991 Free Software Foundation, Inc.,
   51 Franklin Street, Fifth Floor, Boston, MA 02110-1301 USA
   Everyone is permitted to copy and distribute verbatim copies
   of this license document, but changing it is not allowed.
  ```

- Files shipped: the `postgis` extension only — `postgis.control`,
  `postgis--3.6.2.sql`, `postgis--ANY--3.6.2.sql`, `lib/postgis-3.dll`, the library
  closure listed in the manifest, and `share/contrib/postgis-3.6/proj/proj.db`. The other
  extensions the vendor bundle carries (`postgis_raster`, `postgis_topology`,
  `postgis_sfcgal`, `postgis_tiger_geocoder`, `address_standardizer`, `h3`, `pgrouting`,
  `pointcloud`, `mobilitydb`, `ogr_fdw`) are pruned and are not redistributed by Spotledger.
- Source offer (GPL v2 section 3). The complete corresponding source of PostGIS 3.6.2 is
  available from OSGeo:

  ```
  PostGIS 3.6.2 source:
  https://download.osgeo.org/postgis/source/postgis-3.6.2.tar.gz
  ```

  Spotledger distributes the PostGIS binaries unmodified as obtained from the URL in
  `MANIFEST.toml`, and offers the corresponding source at the URL above for three years from
  the date the installer that carries it was distributed. A request may also be sent to the
  address published at <https://postgis.net/> (the project's support and mailing-list page).

## GEOS 3.14 (shipped as `bin/libgeos.dll`, `bin/libgeos_c.dll`)

- Upstream: <https://libgeos.org/> (source: <https://github.com/libgeos/geos>)
- Version: 3.14, as the PostGIS bundle's `version.txt` records (`GEOS VERSION: 3.14`).
- Licence: GNU Lesser General Public License, version 2.1. Text:
  <https://raw.githubusercontent.com/libgeos/geos/3.14.0/COPYING> (fetched 29 Oct 2026), which
  begins:

  ```
                    GNU LESSER GENERAL PUBLIC LICENSE
                         Version 2.1, February 1999

   Copyright (C) 1991, 1999 Free Software Foundation, Inc.
   51 Franklin Street, Fifth Floor, Boston, MA  02110-1301  USA
   Everyone is permitted to copy and distribute verbatim copies
   of this license document, but changing it is not allowed.
  ```

- Source: `geos-3.14.0.tar.bz2` from <https://download.osgeo.org/geos/> (verified present,
  29 Oct 2026). The DLLs are the unmodified files from the PostGIS bundle.
- LGPL 2.1 section 6 obligation: the Spotledger application is not linked against GEOS; GEOS is
  loaded by the `postgis` extension inside the PostgreSQL server process, and the library may be
  replaced by the user by putting a different `libgeos.dll` in the same directory.

## PROJ 8.2.1 (shipped as `bin/libproj_8_2.dll` and `share/contrib/postgis-3.6/proj/proj.db`)

- Upstream: <https://proj.org/> (source: <https://github.com/OSGeo/PROJ>)
- Version: 8.2.1, as the PostGIS bundle's `version.txt` records (`PROJ VERSION: 8.2.1`) and as
  the shipped file name `libproj_8_2.dll` confirms.
- Licence: MIT-style. Text: <https://raw.githubusercontent.com/OSGeo/PROJ/8.2.1/COPYING>
  (fetched 29 Oct 2026), which begins:

  ```
  All source, data files and other contents of the PROJ package are
  available under the following terms.  Note that the PROJ 4.3 and earlier
  was "public domain" as is common with US government work, ...
  ```

  followed by the MIT permission notice ("Permission is hereby granted, free of charge, to
  any person obtaining a copy of this software ...").
- Source: `proj-8.2.1.tar.gz` from <https://download.osgeo.org/proj/> (verified present,
  29 Oct 2026). The resource database `proj.db` is that release's data, unmodified.
- Runtime note (not a licence term): PROJ 8.2 finds its resources through `PROJ_LIB`
  (<https://raw.githubusercontent.com/OSGeo/PROJ/8.2.1/docs/source/usage/environmentvars.rst>);
  the installer therefore sets `PROJ_LIB=<prefix>\share\contrib\postgis-3.6\proj` for the
  database service — see `MANIFEST.toml`.

## GDAL — not shipped

The PostGIS bundle contains GDAL 3.9.2 (`bin/libgdal-35.dll`, `gdal-data/`). Spotledger does
**not** redistribute it: `objdump -p lib/postgis-3.dll` on the real bundle (29 Oct 2026) shows
no GDAL import, so no function the platform uses needs it. Raster I/O and the GDAL-backed
PostGIS functions are consequently unavailable in the desktop bundle, which ADR-087 does not
require. If a later task ships GDAL, its notice (the bundle's `gdal-data/LICENSE.TXT`, MIT-style
with third-party sections) must be added here and the file declared in the manifest.

## PostgreSQL 18.6 (the server the extension runs in)

- Upstream: <https://www.postgresql.org/>, distributed for Windows by EnterpriseDB —
  `postgresql-18.6-1-windows-x64-binaries.zip` (<https://www.postgresql.org/download/windows/>).
- Licence: the PostgreSQL Licence, "similar to the BSD or MIT licenses"
  (<https://www.postgresql.org/about/licence/>). The archive carries the full text as
  `pgsql/server_license.txt`; the installer must place it beside the product (the manifest does
  not declare it as a file because it is outside the extension's own file set — add it to the
  installer's licence page).

## Other DLLs in the shipped closure — notices outstanding

The PostGIS module links, transitively, against the following libraries that the vendor bundle
carries (all unmodified, all from `postgis-bundle-pg18-3.6.2x64/bin`): `libxml2-2`,
`libcurl`, `libssl-3-x64`, `libcrypto-3-x64`, `libsqlite3-0`, `zlib1`, `libtiff-6`,
`libprotobuf-c-1`, `libstdc++-6`, `libgcc_s_seh-1`, `libwinpthread-1`, `libiconv-2`,
`libintl-8`, `libidn2-0`, `libunistring-5`, `libpsl-5`, `liblzma-5`, `libzstd`, `libssh2-1`.

Their upstream versions are not recorded in the bundle's `version.txt`, so no version is
asserted here and no notice text is reproduced: inventing either would be worse than the gap.
Collecting each one's licence text and version from its upstream release is a release blocker
for the installer, recorded under `## Open questions` in the task report (owner: WP-PRD-03).
