# Field catalogue integration (WP-UI-08.T1 + WP-UI-08.T4)

The catalogue consumes public `Spotledger.Client.Metadata` models. It does not
infer permission from values, interpret mask formats, resolve currencies, merge
attachment scopes, or call reveal/create endpoints.

## Host binding

- `FieldControlFactory.Create(FieldModel)` creates an editor by kind/format/hint.
  `Create(field, state)` also applies permission-derived presentation. A masked
  state creates **only** `SpotMaskedField`, regardless of kind: no underlying
  editor or normal-value binding ever receives the separate reveal result.
- `Create(field, state, draft, fieldPath)` additionally loads
  `draft.Currency[fieldPath]` when present. Paths are exact, including child-row
  paths; a symbol is never looked up from `FormatParameters.CurrencyField` here.
- Metadata state is not observable. After document/permission refresh, call
  `GetBinding(control).Load(state, currencyInfo)`; call `CommitTo(state)` before
  saving. Commit refuses to modify masked or noneditable state. The older
  `Load(FieldValue)` overload is for **ordinary values only**, not presentation.
- Recreate controls on masked/unmasked transitions. An ordinary binding given a
  newly masked state scrubs its editor and parser cache before throwing an explicit
  recreation instruction. A previously masked control never becomes an editor.
  On metadata kind/format changes the host likewise recreates the control.
- Set `SpotMaskedField.RevealCommand` and its `CommandParameter` to the app-layer
  audited reveal workflow. The affordance exists only with `CanReveal`. On a
  successful response the host calls `draft.ApplyRevealResponse`, then
  `GetBinding(control).Load(state)` to copy only formatted read-only text into the
  display. The control retains no `FieldState`, draft or revealed `FieldValue`.
- `HideReveal()` replaces displayed text with the original server mask and clears
  selection. Own/ancestor hiding and detach also scrub it. Showing or reattaching
  never restores a reveal. Hidden or detached controls refuse reveal presentation.
  The app still owns the draft's separate transient reveal slot and its lifetime;
  hiding this view never edits, saves, or reloads the draft.
- Set `CreateCommand` / `CreateCommandParameter` on `SpotLookupBox` or
  `SpotMultiSelect`. Only `CanCreateTarget` offers the action; read-only mode and
  `ICommand.CanExecute` suppress invocation. The host opens the workflow, not the
  control. No dynamic target or table-child create permission is guessed.

## Catalogue behaviour

- `SpotCurrencyInput` keeps amount text separate from `CurrencyInfo.Symbol` and
  delegates parsing/precision/scale validation to Metadata. Even 38-digit amounts
  round-trip without native conversion or floating point; missing presentation
  means a plain decimal. Read-only amounts retain their symbol and selectable text.
- Attachment limits bind directly from `FieldState.AttachLimit`. Oversize,
  uncheckable size, and unacceptable type are refused before invoking the uploader,
  with the effective maximum or accepted types in the message. Empty accept means
  any type; the published `image/*`-style subtype wildcard is supported. With no
  limit there is no client check. Chooser/drop APIs have no MIME metadata, so common
  extensions provide advisory MIME hints and unknown extensions remain octet-stream;
  callers of `UploadAsync(UploadFile)` may supply a more accurate content type.
  Server file validation remains authoritative, including actual content/length.
- Closed hints use only compatible catalogue controls: bool `switch`/`checkbox`,
  text `textarea`, select `dropdown`, multiple-select `chips`, and link
  `autocomplete`. Radio/segmented variants and incompatible or unknown hints use
  the existing default. No runtime XAML or dependency is introduced.
- `SpotSelectMultiple` edits **texts**, unlike table-multiselect links. Toggle and
  binding commit produce unique selected values in metadata option order.
- `ILookupSource.SearchResultAsync` returns Metadata's `LookupResult`. Its default
  implementation adapts existing sources to the single-column presentation.
  `NativeLookupSource` round-trips the generated response through the Metadata
  decoder so columns, cells and extras remain distinct. Both pickers display the
  same ordered headers/cells with inherited RTL mirroring; picking keeps id/label,
  never display cells. Keyboard debounce, cancellation and selection rules remain.

## Verification fixtures

`FieldPresentationGallery` joins every existing direction/density gallery section.
It includes mock mask/reveal/hide, symbol/absent/error/read-only currency, rejected
attachments, every hint/fallback, multiple-select chips and column/create pickers.
Headless tests exercise public permission/document/reveal decoders, fake transports,
existing regressions, and raster captures written only under test `bin/` output.
Time controls are unchanged: A2 adds no field-level timezone contract.
