# Field presentation boundary (native API 0.5.0 / ADR-068 A2)

This project remains pure .NET. Renderers consume `FieldModel`, `FieldState`,
`DocumentDraft.Currency`, and `LookupResult`, not generated API models.

- Fetch metadata with `MetaCache`, or decode a complete meta_get JSON response with
  `MetadataResponseDecoder.Decode`. Both retain unknown control-hint text and map
  it to `ControlHint.Unknown`. The generated enum alone discards future text, so
  `MetaModelFactory.Create` cannot reconstruct text already discarded upstream.
- `FieldModel.FormatParameters` exposes currency field, barcode symbology, code
  syntax and rating maximum. `Multiple` applies only to selects; selected values
  are `MultiSelectFieldValue`, never links. Text parsing accepts a JSON array,
  canonicalizes selections into definition order and removes duplicates.
- Decode auth_permissions with `PermissionSnapshot.Decode`, then apply it with
  `draft.ApplyPermissions` **before** loading the document. Permissions cannot be
  inferred from masked-looking strings or numeric permission levels. No snapshot
  means plain legacy defaults, not a guess about authorization.
- A changed opaque permission etag refreshes states and clears transient reveals.
  A missing etag never suppresses a refresh. Revocation scrubs both current and
  original values, including child rows and their revert snapshots. Refetch the
  document after permission changes to get fresh masked text or readable values.
- `MaskedText` is display-only, `Value` and `Original` contain only the null sentinel
  while masked, direct value assignment is refused, and patch writers omit masked
  fields. `CanReveal` is an affordance, not authority. The app performs the audited
  reveal request, then calls `ApplyRevealResponse(path, responseJson)` on success.
  `RevealedValue` is separate, read-only, transient, and never written by a save.
- Use `LoadFrom`/`LoadFromJson` for reads and `LoadSavedFrom` for save response
  document bodies (the response's `data` object). These refresh `_currency` fully,
  including child-row paths, and clear prior reveal results. `MarkSaved()` only
  rebases values when no server body is available; it cannot resolve currencies.
  Currency code/symbol never enter values, unknown properties or patch bodies.
- Attachment limits are the server's resolved answer, not scopes to merge. They
  support a field on its own DocType or an exact child field path on the parent.
  `CanCreateTarget` comes only from the static link target's `rights.create` in the
  same snapshot; a dynamic target is not guessed.
- `LookupResult.Decode` keeps ordered columns and display cells separate from
  copyable extras (retained as exact JSON). An absent columns list is the legacy
  label/description picker. No lookup result decoder or `ILookupSource` exists in
  Controls at this task's base revision; nothing was moved from that project.

The brief's `Controls/FIELD-CATALOGUE-INTEGRATION.md` is absent in the base revision.
App/control wiring of these public models belongs to WP-UI-08.T4. There is no
field-level time zone, mask format or create-capability metadata property.
