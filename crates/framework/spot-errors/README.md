# spot-errors — the error and message catalogue

Every stable dotted code the platform can raise has one entry in `catalogue/`: its
`ErrorClass`, a one-sentence developer-facing summary, the localisation key of its longer
explanation, the parameter names its message uses, a one-sentence remedy, and the version
that introduced it. The files are compiled in with `include_str!` and validated at test time;
`Catalogue::explain` turns an `AppError` into `{ code, summary, remedy, params }` in a
requested language (English until a translation table lands), and
`Catalogue::missing_explanation` is what the engine asserts in debug builds so a code that is
raised but not catalogued is caught before release — the code-completeness scan
(`tools/lints/error-codes/`) checks the same rule across the whole repository, in both
directions.

## How to add a code

1. Raise it in the owning crate first (constructor literal, `code()` match arm, or a `const`
   in a `codes`-style module), with a plain-language message that names no secret and no raw
   SQL (see the rules below).
2. Add one `[[entry]]` to the catalogue file named for the code's prefix — `document.*` goes
   to `catalogue/document.toml` — with every field filled. A new prefix is a new file; `build.rs`
   lists `catalogue/*.toml` at build time, so there is no list in `src/lib.rs` to edit:

   ```toml
   [[entry]]
   code = "document.required"
   class = "Validation"          # an ErrorClass name, or "none" (see below)
   summary = "A required field or child table has no value: {field}."
   explanation_key = "error.document.required"
   params = ["field"]            # exactly the names the summary braces
   remedy = "Fill the named field, or remove the row requirement in the DocType."
   since = "0.1.0"               # the version that introduces the code
   ```

3. Run the scan (`tools/lints/error-codes/run.sh` or
   `python3 tools/lints/error-codes/error_codes_lint.py`) and the crate tests
   (`cargo test -p spot-errors`). Both must pass: the scan fails on any raised code missing
   from the catalogue **and** on any catalogue entry nothing raises (unless the entry sets
   `retired = true`).

When a code stops being raised, do not delete the entry (old clients still report it): set
`retired = true` and keep the summary so a fresh install explains an old code.

## The naming rule

A code is dotted lower-case in the form `spot_app_api::error::is_code` accepts: at least two
segments, each matching `[a-z][a-z0-9_]*` — `document.required`, `step.duplicate`,
`llm.auth`, `licence.clock.rewind_beyond_tolerance`. Never upper-case, never spaces, never a
leading digit, never a segment starting with `_`. The first segment is the code's prefix and
the catalogue file name; one code lives in exactly one file. A code's meaning is stable
within a major version: adding entries is fine, changing a code's summary or remedy is a
review question, recycling a code for new meaning is not allowed.

`class` names the `ErrorClass` the raise site uses (`Validation`, `NotFound`, `Internal`, ...)
when the code rides an `AppError`. Use `none` for codes that never do — registry/wire
vocabularies (`server.maintenance` until something raises it, `http.unknown_site` from the
transport layer), library-internal vocabularies (`storage.*` kinds, `auth.hash.malformed`
before the host maps them onto app-facing codes), non-fatal `Message` codes
(`selling.amount_derived`) and test-suite probe codes (`probe.*`, `l3.*`).

## The message rules

**An error message never contains a secret or a raw SQL string.** A message is logged,
enveloped to clients and stored forever; values like passwords, keys, tokens, sealed payloads
and SQL statements (even when they carry the error) must stay out of it. Name the field, not
the value: "The document was changed by someone else.", not the SQL that failed. Field errors
path-point at the exact cell (`items[2].qty`) instead of echoing content; secret fields carry
`document.secret_not_writable` and are set through the secrets service, never in a document.

Summary and remedy sentences in the catalogue follow the same spirit: one sentence each,
developer-facing, no values, braces naming exactly the `params` the message uses.