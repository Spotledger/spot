# Third-party material in Spotledger.Client.Theme

This assembly carries three font files and their licences. Everything else in it is ours or is a
package reference restored by NuGet (Avalonia, checked by `Directory.Packages.props`).

## Noto Sans (SIL OFL 1.1)

The Latin face. Bundled for the same reason as the Urdu faces — a bare machine renders text
instead of boxes — and because a text-bearing screenshot baseline must not depend on whichever
font a Linux host substitutes for `Inter`/`Segoe UI`.

- **File:** `Fonts/NotoSans-Regular.ttf` (453,428 bytes).
- **Title:** Noto Sans.
- **Authors:** Copyright 2022 The Noto Project Authors
  (<https://github.com/notofonts/latin-greek-cyrillic>).
- **Licence:** SIL Open Font License, Version 1.1 — full text at `Fonts/NotoSans-OFL.txt`
  (`avares://Spotledger.Client.Theme/Fonts/NotoSans-OFL.txt`).
- **Modifications:** none.
- **Scope:** this font file only, not Spotledger code.

## Noto Naskh Arabic (SIL OFL 1.1)

The Urdu fallback the client renders with. Copied, not linked, from the repository's
licensed print assets in `lab/SPK-03/assets/` (the PDF spike, SPK-03); the bytes are
identical to those files.

- **Files:** `Fonts/NotoNaskhArabic-Regular.ttf` (163,400 bytes),
  `Fonts/NotoNaskhArabic-Bold.ttf` (163,504 bytes).
- **Bundle total:** 789,110 bytes (771 KiB) counting the Latin face and both licence texts.
- **Title:** Noto Naskh Arabic.
- **Authors:** Copyright 2022 The Noto Project Authors
  (<https://github.com/notofonts/arabic>).
- **Licence:** SIL Open Font License, Version 1.1 — the full text ships beside the faces as
  `Fonts/NotoNaskhArabic-OFL.txt` and is compiled into this assembly as an Avalonia
  resource (`avares://Spotledger.Client.Theme/Fonts/NotoNaskhArabic-OFL.txt`).
- **Modifications:** none. The files are shipped byte-for-byte as published; we do not
  subset, rename, re-encode or otherwise adapt them. The reserved font name is therefore
  untouched and the OFL's renaming clause does not apply.
- **Scope:** the licence covers these two font files only, not Spotledger code. They are
  read-only resources of the theme assembly.
- **Why Naskh and not Nastaliq:** Naskh has clean, even metrics in dense grids and a small
  file; Noto Nastaliq Urdu's tall line height and larger file make it a poor default for a
  data-entry screen. Nastaliq stays an optional user setting in a follow-up (the token
  `Font.Family.Urdu` already lists it as a lower-priority fallback).

A test (`FontResourcesTests`) fails if a face is missing from the assembly, if the faces stop
matching the files in `lab/SPK-03/assets/`, or if an OFL text is absent.
