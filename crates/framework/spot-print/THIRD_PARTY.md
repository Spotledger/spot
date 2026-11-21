# Third-party material in spot-print

`spot-print` embeds Typst and bundles fonts. This file lists what ships with it
beyond our own code and the notices that licence terms require. Decision: the
architecture owner accepted Typst in SPK-03 and decided on 2026-09-29 to ship
the CC-BY-SA data below unchanged, with a credit notice and no legal review.

## Typst and its dependency graph

`typst`, `typst-pdf` and `typst-layout` 0.15.1 (Apache-2.0) and their
dependencies are linked into the binary. The crate licences are checked by
`cargo deny` against the repository `deny.toml`. SPK-03 found two licences
outside its allow-list: `roman-numerals-rs` (0BSD OR CC0-1.0, shipped) and
`ar_archive_writer` (Apache-2.0 WITH LLVM-exception, build time only). Adding
`0BSD` to `deny.toml` is a lead decision; it is not made here.

## CSL styles and locales (CC-BY-SA 3.0)

`typst-library` enables `hayagriva/archive`, which embeds Citation Style
Language (CSL) data in the binary. We do not use bibliographies, and we cannot
switch the data off (Typst has no cargo features).

- **Title:** the CSL styles and locales in the `archive/` directory of the
  `hayagriva` crate, version 0.10.1.
- **Authors:** the Citation Style Language project and the individual
  contributors named in each style's `<author>`/`<contributor>` metadata
  (styles: <https://github.com/citation-style-language/styles>; locales:
  <https://github.com/citation-style-language/locales>), as redistributed by
  the Typst project in the `hayagriva` crate
  (<https://github.com/typst/hayagriva>).
- **Licence:** Creative Commons Attribution-ShareAlike 3.0 Unported,
  <https://creativecommons.org/licenses/by-sa/3.0/> (hayagriva's `NOTICE` file
  states that the CSL styles and locales in `archive/` are under this
  licence).
- **Modifications:** none. The files are used exactly as published in the
  crate; we do not edit, translate, adapt or re-encode them.
- **Scope:** the licence covers these data files only, not Spotledger code.
  The data is a read-only compiled-in resource and our code is not an
  adaptation of it.

## Bundled fonts (SIL Open Font License 1.1)

Embedded with `include_bytes!` from `fonts/`, unmodified from the Noto
releases used in SPK-03 (static instances, copied from `lab/SPK-03/assets/`).

| Font | Files | Licence text |
| --- | --- | --- |
| Noto Sans | `NotoSans-Regular.ttf`, `NotoSans-Bold.ttf` | `fonts/NotoSans-OFL.txt` |
| Noto Naskh Arabic | `NotoNaskhArabic-Regular.ttf`, `NotoNaskhArabic-Bold.ttf` | `fonts/NotoNaskhArabic-OFL.txt` |

Copyright is held by The Noto Project Authors (see the headers of the licence
files). The fonts are not sold on their own. PDFs we produce embed subsets of
these fonts, which the OFL permits.
