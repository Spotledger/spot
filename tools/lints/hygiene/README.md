# Hygiene scanner — reference harness

`reference.py` is the offline oracle for `hygiene.py`. It fetches a pinned
snapshot of the frappe and erpnext sources into a shared cache, builds a
hygiene index from that corpus, and then proves two numbers on every run:

1. **Recall** — copies of reference code are detected at a fixed rate.
   `--seeded-recall N` deterministically selects N corpus files with a fixed
   mix of Python, JavaScript and DocType JSON (proportions 2:1:1), transforms
   each of them (`rename` whole-file copies, wrapped `fragment` blocks,
   values-only `json` rewrites), scans them through the same index and asserts
   every copy is flagged. Fragment detection relies on contiguous k-gram
   chaining, so selected code files must be long enough for the fragment
   transform (`FRAGMENT_WINDOW_TOKENS`); selection walks a seeded shuffle of
   each bucket and retries past candidates that are too short until the full
   request is filled, then tops up from the other buckets if a bucket runs
   out of eligible files (deterministic for a given pin: same seed, same
   files). The bar is 95% recall per transformation (`MIN_RECALL`); at
   N=40 the per-transformation samples are 10/10/5/5/10, so a single miss
   drops a cell below the bar (9/10 = 90%, 4/5 = 80%) — effectively the bar
   means "no misses" at these sizes. The measured value with the pinned
   corpus is 100% (see the numbers below).
2. **False-positive rate** — `--false-positives` scans this repository
   (`run.sh`'s targets, plus reference-internal files) through the same index
   and asserts the flagged rate stays under the 0.30 threshold. The expected
   value on our own tree is 0%. Scans enumerate git-tracked (or staged) files
   only — `git ls-files --cached` inside a work tree — so untracked build
   output such as generated `obj/project.assets.json` can never produce a
   match; outside git the plain file-system walk is used (the fixture corpus).
   `--include`/`--exclude` are globs over that set, so they cannot reach an
   untracked file inside a work tree; pass `--all-files` to walk the directory
   instead (for example to scan a new file before `git add`).

The pin lives in `reference.toml`: the archive URL, tag, commit SHA and
sha256 digest for each repo. `--check-pin` validates the pin offline (URL
shape, tag/SHA agreement, digest match) without network access, and
`--fetch --build --check-pin` bootstraps the full cache:

| Cost (measured on a 4-core laptop) | |
| --- | --- |
| Cold `--fetch --build --check-pin` (2 archives, download + extract + index) | ≈ 38 s locally, ≈ 33 s on a GitHub runner (build ≈ 24 s of it), peak RSS ≈ 90 MiB |
| Warm `--build` (corpus already extracted) | ≈ 24 s |
| Index size | ≈ 5.5 MB (5,958 indexed of 7,320 extracted files, 4.85 M tokens, 415.9 K fingerprints after low-information filtering — ≈ 8.6% of the 454.9 K unfiltered winnowed positions are dropped) |
| `--seeded-recall 40` with a warm index | ≈ 2 s |
| `--false-positives` on this repository | ≈ 1 s |

Measured seeded recall with the pinned corpus: 40/40 copies detected
(10 rename python, 5 rename javascript, 10 fragment python, 5 fragment
javascript, 10 json-values) — unchanged by the low-information filter,
because surrounding non-repeating code keeps a winnowed window alive for
every w-gram span of a real copy. Measured false-positive rate on this
repository with an empty `allow.toml`: 0 of 106 tracked-or-staged files
flagged (0.00%) — the three datagen false positives (a tuple of invented
syllables in `harness/datagen/datagen/names.py`, the repeated `assertEqual`
runs in `harness/datagen/tests/test_distribution.py` and
`harness/datagen/tests/test_verify.py`) are now filtered structurally; see
`allow.toml` for their history. The green cold-path CI run
(empty actions cache) measured on the runner: 33.4 s fetch-and-build,
2.9 s seeded recall, 0.85 s false-positive scan (80 files).

## Low-information fingerprints

A winnowed window is dropped at fingerprint time — on the corpus side during
`build_index` and on the target side during scanning — when it is too generic
to identify a real copy on its own (`hygiene.window_is_low_information`):

1. **Literal-only windows** carry no identifier, keyword or JSON key: every
   token collapses to `LIT`, a template/interpolation marker or a JSON
   structure marker. This kills matches for files whose whole body is a tuple
   of invented syllables or long string runs.
2. **Period-dominated windows**: some period `p <= LOW_INFO_MAX_PERIOD` (16)
   repeats over at least 90% of the window's aligned token positions
   (`LOW_INFO_PERIOD_FRACTION`), as in runs of a single assertion-statement
   shape. Tokenising keeps identical-shaped statements indistinguishable
   (identifiers collapse to `ID`, literals to `LIT`), so such runs match
   arbitrary unrelated code. `JSON_ITEM` values are line numbers, which also
   repeat — structure-only windows are periodic, not informative.

The filter applies while both files are indexed and while scanning, so a
boilerplate-only window can never become a coverage or contiguous-region
match, and matches never *grow* through it (contiguous chains require pairs
forming on both k-grams). The warm index build measured 25.9 s with the
filter versus ≈ 24 s before — within run-to-run variance — and the index
shrinks ~9%. Existing cache indexes built with older code stay readable —
CI rebuilds whenever `hygiene.py` changes (the cache key hashes the
normaliser file) — but an old index retains the extra fingerprints; oracle
steps there still pass because target-side files drop the same
low-information windows.

## Cache layout

```
${SPOT_HYGIENE_CACHE:-${XDG_CACHE_HOME:-~/.cache}/spot-hygiene}
├── archives/   # pinned source archives, named by digest
├── corpus/     # extracted frappe/erpnext trees (git-ignored, shared)
└── index-<sha>.bin   # one index per pinned corpus, named by the pin SHA
```

## Usage

```sh
# validate the pin (no network)
python3 tools/lints/hygiene/reference.py --check-pin

# one-time bootstrap: download, verify, extract, index
python3 tools/lints/hygiene/reference.py --fetch --build --check-pin

# oracle checks (need the index)
python3 tools/lints/hygiene/reference.py --seeded-recall 40 --index "$(python3 tools/lints/hygiene/reference.py --print-index-path)"
python3 tools/lints/hygiene/reference.py --false-positives --index "$(python3 tools/lints/hygiene/reference.py --print-index-path)"
```

CI decides whether to bootstrap with `--print-index-path`, which prints the
expected index path unconditionally and exits 0 even when nothing has been
built yet; the shell checks whether the printed file exists:

```sh
index=$(python3 tools/lints/hygiene/reference.py --print-index-path)
if [ ! -f "$index" ]; then
  python3 tools/lints/hygiene/reference.py --fetch --build
fi
```

`run.sh` wires the scan up: with `SPOT_HYGIENE_INDEX` set it scans the
repository against the pinned index; without it, everything is inert so
plain invocation never fails. The oracle steps (`--seeded-recall`,
`--false-positives`) are separate CLI calls — see the Usage block and the
CI workflow for the wiring.

Unit tests (`python3 -m unittest discover -s tools/lints/hygiene/tests`)
cover pin loading/digest parsing, seeded selection determinism and mix,
transform behaviour, and an end-to-end rename-copy detection against an
in-memory mini index; `tests/test_precision.py` pins the false-positive
fixes (literal-only and period-dominated windows produce no fingerprints on
either side, realistic modules still fingerprint and trace renamed copies,
and default scans cover only git-tracked or staged files). They never touch
the network or the shared cache.

CI (`.github/workflows/hygiene.yml`) restores the spot-hygiene cache (keyed
by the pin SHAs plus a hash of the normaliser code) and runs the scan on
every pull request, every push to `main`, weekly and on demand. Seeded
recall and the false-positive run execute on pushes, dispatches and pull
requests that touch `tools/lints/hygiene/**`. It is **not** a required
check yet: the lead promotes it once the false-positive rate is known.