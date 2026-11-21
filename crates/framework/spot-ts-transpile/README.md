# spot-ts-transpile (WP-EXT-08.T2)

ADR-054 step 2: on save, TypeScript is stripped to plain JavaScript for the
engine tier. This crate never runs or type-checks anything: type *checking*
is the editor integration (WP-EXT-09, .d.ts from DocType metadata); execution
is QuickJS via `rquickjs` (`spot-ts`, WP-EXT-08.T1).

## Why oxc (and not swc)

`oxc_*` 0.153 is preferred over `swc_*` and that choice stays behind this
crate's API (`transpile`, `Transpiled`, `Diagnostic`, `Options`):

- **Small dependency tree**: oxc is one workspace of pure-Rust crates
  (`oxc_parser`, `oxc_transformer`, `oxc_codegen`, `oxc_sourcemap`); no
  prebuilt blobs, no Node. The engine tier's own choice (rquickjs/QuickJS,
  plain C) stays the only foreign build on Windows and Linux.
- **Single-pass**: parser, transformer and codegen share one arena; the
  transformer is explicitly modelled on Babel's preset-typescript behaviour
  (enums to objects, `declare` removed, `import type` removed).
- **Deterministic output** with stable options: no lowering by default, so
  the emitted JavaScript is what the author wrote minus the types.

## What the output is

Call `transpile(source, file, &Options::default())`:

- `js` — ESM JavaScript at the author's syntax level. The engine (QuickJS)
  supports ES2020+; the transpile does not downlevel classes/optional
  chaining/etc., it only removes types (the engine's target is documented
  here: **ES2020 or later, unchanged**). Enums become the equivalent object;
  `const enum` uses are inlined away; `as`/`satisfies`/generics/interfaces/
  type aliases/`declare`/non-null assertions vanish; `import type` is removed.
- `source_map` — standard v3 JSON (`version`, `sources`, `sourcesContent`,
  `mappings`). `sources` carries the `file` argument as given; absolute
  paths are reduced to their final component. Two runs on the same input
  produce byte-identical `js` and `source_map` (asserted by tests).
- `imports` / `exports` — the module links the *emitted* JavaScript makes
  (type-only imports are neither linked nor reported).

## The tier rules (checked on save, before the engine)

| Code | Construct |
| --- | --- |
| `ts.syntax` | A syntax error (all of them, when the parser recovers) |
| `ts.import_not_allowed` | An import / `export ... from` of a module outside `Options::allow_imports` (default `spot:*` — the engine's virtual modules; there is no module loader) |
| `ts.dynamic_import_not_allowed` | `import(...)` |
| `ts.eval_not_allowed` | `eval(...)` or any reference to `eval` |
| `ts.require_not_allowed` | `require(...)` (including `import x = require(...)`), or any reference to `require` |
| `ts.function_constructor_not_allowed` | `new Function(...)` / `Function(...)` |
| `ts.with_not_allowed` | A `with` statement |
| `ts.decorator_not_supported` | Any decorator |
| `ts.namespace_not_supported` | `namespace Foo {}`, `module Foo {}`, `import x = Namespace` |
| `ts.source_too_large` | Source over `Options::source_size_cap` (1 MiB default) |
| `ts.too_deep` | Bracket nesting over `Options::nesting_depth_cap` (128 default) |

Caps are enforced by a linear pre-scan (no regex, no recursion) so hostile
input is rejected in bounded time; the parser then sees only small, shallow
sources.

## Layout

- `src/transpile.rs` — the flow and the public types.
- `src/collect.rs` — the single AST walk for tier rules + module records.
- `src/limit.rs` — the nesting-depth pre-scan.
- `src/diagnostic.rs` — `Diagnostic` + the 1-based `LineIndex`.
- `src/options.rs` — `Options`.
- `tests/` — the corpus walker (`tests/corpus/` holds `input.ts` plus either
  `expected.js` (accepted) or `expected.diagnostics.json` (rejected),
  determinism, source-map spot checks, the reject rules, and a proptest that
  arbitrary strings never panic. `SPOT_GOLDEN_WRITE=1` regenerates the
  corpus goldens; CI never sets it.