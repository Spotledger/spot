# Urdu and right-to-left in the Avalonia client

The Urdu RTL bullet of WP-UI-15 asks for screens that are *correct*, not merely mirrored
containers: real Naskh glyphs on a machine with no Urdu font, digits the reader expects, and
mixed Urdu/Latin/number fields that do not reorder themselves. This document is the rule
book the tests in `tests/Spotledger.Client.Theme.Tests` enforce.

## 1. The font is bundled, and it is the fallback that wins

`clients/avalonia/src/Spotledger.Client.Theme/Fonts/` carries three SIL OFL 1.1 files copied
(unmodified) from the repository's licensed print assets in `lab/SPK-03/assets/`:

| file | bytes | role |
| --- | --- | --- |
| `NotoNaskhArabic-Regular.ttf` | 163,400 | Urdu body text |
| `NotoNaskhArabic-Bold.ttf` | 163,504 | Urdu emphasis, table headers |
| `NotoNaskhArabic-OFL.txt` | 4,382 | the licence text, shipped beside the faces |
| `NotoSans-Regular.ttf` | 453,428 | Latin text and Latin numerals |
| `NotoSans-OFL.txt` | 4,396 | the licence text, shipped beside the face |

789,110 bytes in total (771 KiB). They are `AvaloniaResource`s, so they travel inside
`Spotledger.Client.Theme.dll` and are addressed as
`avares://Spotledger.Client.Theme/Fonts/NotoNaskhArabic-Regular.ttf`. `THIRD-PARTY-NOTICES.md`
beside the project records the attribution, the licence and the no-modification statement.

Three pieces make the bundle the face the client actually draws with:

1. `SpotledgerFonts.Register()` adds an `EmbeddedFontCollection` keyed `fonts:SpotledgerNaskh`
   over that folder. Avalonia requires the `fonts:` scheme for a custom collection key; the
   family `Noto Naskh Arabic` then resolves by *name* through the process font manager.
2. The token `Font.Family.Urdu` is `fonts:SpotledgerNaskh#Noto Naskh Arabic` — one collection,
   one family, **no by-name fallbacks**. That is deliberate: the field label asks for
   `FontWeight.Medium`, which the bundle does not have (Regular and Bold only), and with a
   by-name entry such as `Noto Naskh Arabic` after the collection Avalonia answered that weight
   from a *host* family of the same name. The Ubuntu CI image ships its own Noto Naskh Arabic,
   so the label rasterised with a different version of the face than on a machine without one:
   the same screen, two sets of glyphs, and 774–2,974 pixels of difference per baseline. Naming
   only the collection makes a missing weight simulate from the bundled faces instead.
3. `SpotledgerTheme.MergeTokenResources` calls `Register()`, so a host that installs the
   tokens cannot forget the fonts.

**Why Naskh and not Nastaliq.** Naskh keeps even metrics in a dense grid and is 163 KB;
Nastaliq's tall line height inflates every row and its file is several times larger. Nastaliq
stays as a lower-priority entry in the token list and as an optional user setting in a
follow-up, not as the default.

**The Latin boundary, and why Noto Sans ships too.** Noto Naskh Arabic is an Arabic-script font:
it has no Latin letters, no `-`, `+`, `(`, `)` or `@`. A mixed field is therefore *not* rendered
with the Urdu family alone — Latin runs resolve through the Latin family. `Noto Sans` is bundled
for the Latin side, and `SpotledgerFonts.Latin` exposes it as
`fonts:SpotledgerSans#Noto Sans, Noto Sans, Inter, Segoe UI, sans-serif`.

Two reasons. A bare machine then has a real Latin face instead of boxes for the numbers in an
Urdu screen, and a text-bearing screenshot baseline can pin its Latin font. That second reason is
not theoretical: the first version of this matrix generated baselines with whatever the local
Linux host substituted for `Inter`, and CI rejected every text-bearing case by 332–2,974 pixels
(0.5–4.4%) because the runner substitutes a different family. The test application therefore
resolves `Font.Family.Latin` to the bundled family, exactly as `docs/testing.md` requires a
text-bearing baseline to fix its fonts. The product token still prefers `Inter`/`Segoe UI`; the
bundle is the deterministic fallback.

The Latin family is strict for the same reason: `SpotledgerFonts.Latin` is
`fonts:SpotledgerSans#Noto Sans`.

`MixedFieldsOnlyLackLatinCoverageWhichTheFallbackFamilySupplies` asserts the boundary itself:
every code point the Urdu face lacks in the mixed fixture is ASCII, and no Arabic-script
character is ever uncovered.

## 2. The no-tofu check

`UrduShapingTests.EveryUrduStringShapesWithoutAMissingGlyph` shapes all **367** strings of
`Resources/UrduResourceSet.json` with the bundled face through `TextShaper.Current` — the same
HarfBuzz/Skia pipeline the application uses — with `bidiLevel: 1` and the `ur-PK` culture.
It fails, naming the string id and the code point, on:

- any glyph that comes back as `.notdef` (glyph index 0), and
- any character with no cmap entry (`GlyphTypeface.CharacterToGlyphMap.ContainsGlyph`).

The face is passed explicitly instead of letting the font manager choose, so a system Arabic
font on the test machine cannot hide a gap in the bundle.

The fixture spans UI chrome (150 strings), ERP labels (95), personal names (50), addresses
(30), months and weekdays (22), currency and unit names (20) — a *test fixture*, not the
product's translation catalogue, which is a separate package the brief puts out of scope.

## 3. Numerals

`NumeralSystem { Latin, ArabicIndic, ExtendedArabicIndic }` is a **display** setting; the
default is `Latin` because ledgers and bank documents in Pakistan are written with Latin
digits. `NumeralText` normalises typed input and `NumeralFormat` renders it:

```csharp
NumeralFormat.For(NumeralSystem.ExtendedArabicIndic).Money(1234.5m); // ۱٬۲۳۴٫۵۰
NumeralText.NormalizeNumericInput("١٢٣٫٤٥");                          // "123.45"
NumeralText.TryParseDecimal("۱۲۳٫۴۵", out var value);                 // 123.45m
NumeralText.TryNormalizeNumericInput("1,5");                          // ("1,5", AmbiguousSeparator)
```

Rules that hold and are tested:

- Parsing never leaves `decimal` (ADR-12). No `double`, no `float`, no `Math.Round` in the
  numeral or bidi primitives — a source scan enforces it.
- The Arabic decimal separator U+066B and thousands separator U+066C are recognised on input
  and produced on display; U+2212 (the minus a Persian keyboard emits) is accepted.
- The OS culture is irrelevant: the same text gets the same answer under `de-DE` as under
  invariant, including the refusals.
- Display rounds nothing: a value with more fraction digits than the scale is shown verbatim.
- `NumeralSettings` defaults to Latin and round-trips its storage value; an unknown or missing
  stored value falls back to the default instead of failing a start-up.

### 3.1 A separator is counted only where the grouping is well formed

The old rule deleted every `,` and NBSP before parsing, so `1,5` — the decimal comma a great
many keyboards and imported documents use — committed as **15**: ten times the amount, with no
message. In a ledger that is the worst possible failure, so the rule is now **no guessing**: a
value that could be read two ways is refused and the input says why.

`NumeralText.TryNormalizeNumericInput` returns `NumericInputResult { Normalized, Problem }`.
`Normalized` is the canonical Latin edit text when `Problem` is `None`, and **the original text
unchanged** otherwise, so the control can keep what the user typed and quote it back.
`NormalizeNumericInput` is the thin wrapper (`Normalized`, which is the original on a refusal),
and `TryParseDecimal` returns `false` for any problem instead of parsing a guess.

Recognition, in this order:

1. **Digits.** Arabic-Indic (U+0660–U+0669) and Extended Arabic-Indic (U+06F0–U+06F9) map to
   Latin. The decimal point is U+066B or `.`; the minus is U+2212 or `-`, and only in the first
   position. A second sign or a second decimal point is `UnknownCharacter`.
2. **Grouping candidates** are `,`, U+066C, U+060C, NBSP (U+00A0), U+202F and the space. A
   candidate is valid **only in the integer part** and **only if the groups are well formed**:
   - Western: a first group of one to three digits, then groups of three — `1,234`,
     `12,345,678`;
   - South Asian (the grouping Pakistani documents use): a first group of one or two digits,
     then groups of two, and a **last group of three** — `12,34,567`, `1,00,000`.
3. **`AmbiguousSeparator`** — a comma-family candidate (`,`, U+066C, U+060C) with a run of
   digits after it that is not exactly three, anywhere in the text: `1,5`, `1,50`, `12,5`,
   `12,34`, `1,5000`, and the European `1.234,50`. It could be a decimal comma, so it is
   refused; the text is returned as typed.
4. **`MisplacedGrouping`** — a candidate in the wrong place that is not ambiguous: inside the
   fraction part (`1.234,567`), an empty group (`1,,234`), a leading or trailing separator
   (`,5`, `1,`), a non-comma candidate that does not group properly (`1 5`), or a shape that is
   neither Western nor South Asian (`1234,567`, `1,2,3`).
5. **`UnknownCharacter`** — anything else that is not a digit, sign, point or candidate, a sign
   that is not first (`--5`), or a second decimal point (`12,34.5.6`).
6. **`Empty`** — nothing numeric typed yet: null, blank, or a lone sign or point.

### 3.2 The twelve inputs the tests pin

`NumeralTests.AmbiguousSeparatorsAreRefusedInsteadOfGuessed` is this table, row for row.

| input | result | `Problem` | why |
| --- | --- | --- | --- |
| `1,5` | `1,5` (unchanged) | `AmbiguousSeparator` | one comma, one digit after it: could be `1.5` |
| `1,50` | `1,50` (unchanged) | `AmbiguousSeparator` | one comma, two digits after it |
| `1,500` | `1500` | `None` | well-formed Western grouping |
| `12,34,567` | `1234567` | `None` | well-formed South Asian grouping |
| `1,2,3` | `1,2,3` (unchanged) | `MisplacedGrouping` | groups of one digit: neither style |
| `١٬٢٣٤٫٥` | `1234.5` | `None` | Arabic-Indic digits, U+066C and U+066B |
| `۱٬۲۳۴٫۵` | `1234.5` | `None` | Extended Arabic-Indic digits |
| `1.234,50` | `1.234,50` (unchanged) | `AmbiguousSeparator` | European spelling: the comma follows the decimal point with two digits after it |
| `1,` | `1,` (unchanged) | `MisplacedGrouping` | separator with no digits after it |
| `--5` | `--5` (unchanged) | `UnknownCharacter` | a sign is only valid in the first position |
| (empty) | `` | `Empty` | nothing typed |
| `abc` | `abc` (unchanged) | `UnknownCharacter` | not a number at all |

**Inputs that changed meaning.** `1,5` was `15`, `1,50` was `150` and `12,34` was `1234`; all
three are now refusals. `12,34.50` was `1234.50` and is now refused for the same reason — the
South Asian spelling of that amount is `12,34,567.50`-shaped, so its last group must be three
digits. `1.234,50` was `1234.50` and is now refused.

### 3.3 The refusal is a message key, not a sentence

`NumeralText.Explain(NumericInputProblem)` returns a **key** from `NumericInputKeys`, so this
assembly carries no user-facing English and the wording stays with the translators (Urdu and
English catalogues). `None` has no message and yields the empty string.

| `Problem` | key |
| --- | --- |
| `Empty` | `numeric.empty` |
| `MisplacedGrouping` | `numeric.misplaced_grouping` |
| `AmbiguousSeparator` | `numeric.ambiguous_separator` |
| `UnknownCharacter` | `numeric.unknown_character` |

`NumeralTests.EveryProblemCarriesItsOwnMessageKey` holds that every problem has its own key and
that each key is a key (`numeric.` and lowercase words), not prose.
`NumeralTests.MoneyFormattingAndParsingRoundTripInBothGroupingStylesAndAllDigitSets` formats 200
random amounts with `NumeralFormat.Money` in all three digit sets, and re-groups each the South
Asian way, and parses both back to the same `decimal`.

`NumeralTests.EasternDigitsRoundTripThroughTheDecimalInputToTheSameDecimal` drives a real
`SpotDecimalInput`: both Eastern spellings, normalised by `NumeralText`, commit as `123.45m`.

**Done in the control (WP-UI-15.T5).** `SpotDecimalInput` now calls the normaliser itself, in
two places.

- **Typing.** `OnTextInput` maps every character through the numeral mapping *before* the
  existing edit filter: Arabic-Indic (U+0660–U+0669) and Extended Arabic-Indic (U+06F0–U+06F9)
  digits become ASCII digits in the box, U+066B becomes the decimal separator the control edits
  with (`.` for the default `NumberFormat`, `,` for the European one) and U+2212 becomes `-`.
  The mapping is one character in, one character out, so a keystroke inserts exactly one
  character and the caret, the selection and undo behave as they do for ASCII typing — in a
  left-to-right and a right-to-left window alike. Typed Arabic thousands separators (U+066C,
  U+060C) are accepted and dropped like any other grouping separator: grouping is a display
  concern and `DisplayText` re-groups the committed value.
- **Pasting and committing.** `Commit`, and a multi-character text input (how a paste arrives),
  go through `NumeralText.TryNormalizeNumericInput` — after `ToCanonicalNumericText` maps the
  control's own separators onto `.` and `,`, so a European-configured field reads `1.234,50`
  the way a default field reads `1,234.50`. A `MisplacedGrouping` or `AmbiguousSeparator`
  problem **refuses** the commit: `ErrorText` becomes `NumeralText.Explain(problem)` (the
  `numeric.*` key the catalogue translates), the last good value is restored and `Value` is not
  changed. Concretely, pasting `1,5` is refused with `numeric.ambiguous_separator` instead of
  committing 15, while Western (`1,234.50`) and South Asian (`12,34,567.89`) grouping are
  accepted.
- **Display is unchanged.** `DisplayText` still formats through `NumberFormat`; the
  numeral-system setting continues to change only how a value is *shown* elsewhere, never what
  this control stores or commits.

What pins it, in `SpotDecimalInputTests` (`clients/avalonia/tests/Spotledger.Client.Controls.Tests`):

| test | what it proves |
| --- | --- |
| `EveryArabicIndicDigitTypedOneByOneBecomesTheAsciiDigit` | all ten Arabic-Indic digits, typed one at a time, accumulate as `0123456789` |
| `EveryExtendedArabicIndicDigitTypedOneByOneBecomesTheAsciiDigit` | the same for the ten Extended digits |
| `TypedArabicIndicNumberWithTheArabicDecimalSeparatorCommitsTheSameDecimal` | `١٢٣٫٤٥` typed character by character shows `123.45` and commits `123.45m` at scale 2 |
| `TypedExtendedArabicIndicNumberWithTheArabicDecimalSeparatorCommitsTheSameDecimal` | the same for `۱۲۳٫۴۵` |
| `TypedPersianMinusBecomesTheMinusAndKeepsTheSign` | U+2212 followed by `۷` gives `-7` and commits `-7.00m` |
| `PastedExtendedArabicIndicGroupedNumberCommits` | pasted `۱٬۲۳۴٫۵` commits `1234.5m` |
| `PastedSouthAsianGroupingIsAccepted` | `12,34,567.89` commits `1234567.89m` |
| `PastedAmbiguousSeparatorIsRefusedWithItsMessageAndKeepsTheValue` | `1,5` is refused with `numeric.ambiguous_separator`, `Value` stays `10m` and the box is restored to `10.00` |
| `AmbiguousPasteThroughTheTextInputPathIsRefusedInsteadOfBecomingFifteen` | the same refusal when the paste arrives as one multi-character text input rather than as field text: nothing is inserted, nothing becomes 15 |
| `CaretFollowsTheInsertedDigitWhenTypingInsideANumberInARightToLeftWindow` | in an RTL window, `۷` typed at offset 2 of `12.50` gives `127.50`, caret 3, no selection, commit `127.50m` |
| `BoundsStillApplyAfterNormalisation` | `Min`/`Max` are checked on the normalised value: `۱۲۳٫۴۵` commits, `۱٬۲۳۴٫۵` and `٠٫٥` are refused with the existing bound messages |

`NumeralTests.EasternDigitsRoundTripThroughTheDecimalInputToTheSameDecimal` in the Theme tests
still normalises with `NumeralText` before it sets `Text`. That pre-step is now redundant — the
control normalises on commit — but the file belongs to the Theme owner, so WP-UI-15.T6 may
simplify it.

## 4. Bidi in mixed fields

Two different questions, two different helpers.

**Which base direction does a field have?** `BidiText.FieldDirection(kind, text, fieldDirection)`
answers from the *field kind*, never from the window alone:

- `Code`, `InvoiceNumber`, `Iban`, `Phone`, `Email`, `Money`, `Date` are always
  `LeftToRight`. An IBAN column stays LTR in an Urdu screen.
- `Label`, `FreeText`, `Address` follow their own first strong character (UAX #9 P2/P3, so a
  leading number does not flip an Urdu sentence) and fall back to the field's language
  direction when the text has no strong character at all.

**How is a code-like value shown inside Urdu text?** It is isolated for display with the
Unicode isolates the brief names, U+2066 (LRI) … U+2069 (PDI). `BidiText.ForDisplay` does that
for the code-like kinds; `BidiText.IsolateCode` strips leading and trailing neutral punctuation
out of the isolate so a sentence-final `۔` or `.` stays with the sentence.

Isolates are display-only. `BidiText.RemoveIsolates` restores the exact original, and the
stored value, the copied text and the round trip are unchanged — case 12 asserts it.

Caret and selection are a control concern: because the isolates are inserted only in the
rendered string, a text control's logical offsets never shift. A control that renders
`ForDisplay` output into an editable box must therefore keep the stored value in its editing
buffer (the display string is for labels, cells and read-only surfaces). Recorded as a
follow-up for the Controls owner.

### The twelve cases

| # | Case | Rule asserted | Test |
| --- | --- | --- | --- |
| 1 | Urdu caption in left-to-right chrome | Base direction from the text itself | `Case01UrduCaptionIsRightToLeftInsideLeftToRightChrome` |
| 2 | Empty field, bare number, or punctuation only | Falls back to the field's language direction | `Case02EmptyNumbersAndPunctuationFallBackToTheFieldDirection` |
| 3 | Urdu sentence that starts with a number | A leading digit does not flip the run | `Case03ALeadingNumberDoesNotFlipAnUrduSentence` |
| 4 | Item code inside Urdu text | Isolated LTR, round-trips unchanged | `Case04ItemCodeIsIsolatedLeftToRight` |
| 5 | Invoice number | Isolated LTR, hyphens keep their order | `Case05InvoiceNumberIsIsolatedLeftToRight` |
| 6 | IBAN and phone number | Isolated LTR; a leading `+` does not move | `Case06IbanAndPhoneStayLeftToRight` |
| 7 | ISO date (ADR-077) | Isolated, year cannot jump to the left | `Case07IsoDateIsIsolated` |
| 8 | Money amount | Group separators stay inside the isolate | `Case08MoneyIsIsolatedSoGroupSeparatorsDoNotReorder` |
| 9 | Sentence-final punctuation after a code | Punctuation stays outside the isolate | `Case09TrailingPunctuationStaysOutsideTheIsolate` |
| 10 | Urdu prose vs. an e-mail field | Prose untouched; the e-mail field isolated | `Case10ProseIsUnchangedAndTheEmailFieldIsIsolated` |
| 11 | Urdu address vs. a code column | Address follows its text; the code column does not | `Case11AddressFollowsItsTextWhileACodeColumnStaysLeftToRight` |
| 12 | Stored value and copied text | Isolates never change the value | `Case12IsolatesNeverChangeTheStoredValueOrCopiedText` |

## 5. The Urdu RTL screen matrix

`UrduScreenMatrixTests` renders the controls that exist today in Urdu, right-to-left, at both
densities, in Light and Dark, plus the two high-contrast palettes for the field controls.
Baselines are committed in `tests/Spotledger.Client.Theme.Tests/Baselines/` and named per
`docs/testing.md`: `urdu-field-decimal-480x140-compact-dark-rtl.png` and
`…-compact-highcontrast-rtl.png`.

| case | sizes | variants | densities |
| --- | --- | --- | --- |
| `urdu-field-text` | 480×140 | Light, Dark, both HC | Comfortable, Compact |
| `urdu-field-decimal` | 480×140 | Light, Dark, both HC | Comfortable, Compact |
| `urdu-field-select` | 480×140 | Light, Dark | Comfortable / Compact |
| `urdu-field-checkbox` | 480×140 | Light, Dark, both HC | Comfortable / Compact |
| `urdu-form` (five fields: name, invoice number, amount, payment method, approval) | 640×420 | Light, Dark | Comfortable, Compact |
| `shell-chrome` (the signed-in `ShellView` over the fake server, four tabs; `Shell.Tests/ShellRtlMatrixTests`) | 960×600 | Light, Dark, HC (Comfortable only) | Comfortable, Compact (HC: Comfortable) |

The shell baselines live in `tests/Spotledger.Client.Shell.Tests/Baselines/` (the recipe and its
fake server are in that project), use the bundled Noto Sans, and a second test captures the shell
twice in one process and compares the bytes. They replace the uncompared `07-rtl.png` capture as the
RTL evidence; `ScreenshotParityTests` is unchanged.

The Dark (and HC) `shell-chrome` baselines were re-recorded for WP-UI-15.T9: the chrome built its
tab chips, the tab-content heading/body and the launcher/sidebar text in code with a brush baked
once from whatever theme variant was ambient at construction time, before the host window's own
variant was applied, so Dark painted dark text on its own dark surface. `Shell.Tests/ShellContrastTests`
now asserts WCAG contrast (≥4.5:1 text, ≥3:1 for the ≥24px heading) from the resolved, live brush of
the heading, body text and the active/inactive tab labels against their own background in Light,
Dark and HighContrastDark, independent of any screenshot baseline, so this class of regression fails
even if a baseline is (wrongly) re-recorded from a broken frame again.

The date field is asserted structurally, not in pixels: `SpotDateInput`'s template draws its
calendar affordance as the emoji U+1F4C5, and an emoji glyph comes from whatever emoji font the
host installs, so the same screen rasterises differently on every machine (this is what made the
`urdu-form` case differ by exactly the date row's pixels until the date field left the form). A
pixel case for it becomes possible when the control uses a vector icon or a bundled glyph; that is
the Controls owner's call.

`TheFieldLabelAndInputSwapSidesBetweenDirections` is the mirrored-layout assertion the testing
guide requires: it compares the laid-out screen coordinates of a field's label and input in both
directions, instead of trusting a file-name suffix.

The comparison reuses WP-UI-16.T1's `ScreenshotAssert.Capture/Compare` with the same tolerance,
`SPOT_UPDATE_SHOTS` switch and actual/expected/diff plus HTML review artefacts. It adds the
variant dimension that `ScreenshotMatrix` does not have; folding a variant parameter into the
shared record is a change in `Spotledger.Client.TestSupport`, which this task does not own.

Two host facts the matrix has to handle, both documented in the test code:

- The variant is applied to the **application** as well as the window: `SpotGrid` resolves its
  token brushes against `Application.ActualThemeVariant`, so a window-only variant paints a dark
  screen with a light grid.
- Frames are captured only once two consecutive pumps are byte-identical. A first frame can be
  a half-arranged one (text measured before the glyph cache is warm), which would otherwise make
  a baseline depend on test order.

### The grid

`urdu-grid-720x260-{comfortable,compact}-{light,dark}-rtl.png` show an Urdu header with mixed
rows (Urdu names, Latin item codes, numbers). They were once missing because `SpotGrid` painted a
short grid (4 rows in a viewport of 6) as loading placeholders: the window scheduler only treated
a view as covered when the applied window held `VisibleRowCount` rows, which a grid shorter than
its viewport can never satisfy. The scheduler now clamps the view to the known total
(`WindowScheduler.Covered`). `TheGridHasAnUrduHeaderAndMixedRows` remains as a pixel-independent guard.

### What is not in the matrix, and why

- **Nothing font-related.** Every glyph in every baseline comes from a bundled face and the
  theme's own tokens, so a baseline no longer depends on the runner's font package.
