# Dependency policy

WP-PRD-08 (Security). What the repository checks about its dependencies, which
findings block, and how exceptions are granted. The checks are scripts in
`tools/security/`; the wire-up in CI and the release build belongs to the lead.

## What is checked, and when

| Check | How | When |
| --- | --- | --- |
| Rust advisories, licences, banned crates, sources | `cargo-deny check advisories licenses bans sources`, settings in `deny.toml`; `tools/security/audit.py` wraps and reports it | Every PR and push to `main` |
| NuGet vulnerable packages, direct and transitive | `dotnet list <project> package --vulnerable --include-transitive` for each project in `clients/avalonia/Spotledger.Client.sln`; also wrapped by `audit.py` | Every PR and push to `main` |
| Software bill of materials | `tools/security/sbom.py --out <path>` emits one deterministic CycloneDX 1.5 JSON for the whole repository | Every release build, as an artefact next to the binaries |

A missing tool is skipped with a clear message and a zero exit code so a
developer without the full toolchain is not blocked; `--strict` turns skips
into exit code 2. Locally: `python tools/security/audit.py` and
`python tools/security/sbom.py --out sbom.json`.

## Severity thresholds

`audit.py` fails (exit 1) when any un-suppressed finding is at or above
`--fail-on` (default `high`), or when the allowlist holds an expired
exception; exit 2 means the audit could not complete. Lower severities are
reported but do not block a PR.

- Advisory severities come from the tools; tool levels backstop the scale:
  an error-level cargo-deny diagnostic grades `high`, a warning-level one
  `low`; dotnet `Moderate` maps to `medium`.
- Licence, ban and source rules in `deny.toml` are policy decisions: a finding
  from one of them is treated as `high`, not tunable by a severity flag.

## Exceptions

Exceptions are time-boxed, one entry per advisory, in
`tools/security/allowlist.toml`:

| Key | Meaning |
| --- | --- |
| `id` | The advisory id (RUSTSEC-…, GHSA-…) or the deny.toml diagnostic code |
| `package` | The package the finding is about |
| `reason` | Why it is accepted for now; concrete, with a link |
| `owner` | The lead engineer who approved it |
| `expires` | ISO date, at most 90 days after approval |

Rules:

1. The lead grants every exception; an agent never edits the allowlist without
   that approval — the request goes to the lead on the task issue or `## Open questions`.
2. Maximum 90 days, no renewals of the same entry without a fresh reason.
3. An active exception suppresses matching findings; an **expired** exception
   makes the audit fail until it is removed or renewed.

## Adding a dependency

Per the project coding rules, a dependency (Rust crate, NuGet package, Python library)
is added only when the task brief allows it. Otherwise raise it under
`## Open questions` and stop. The licence bar is the `[licenses]` allow list in
`deny.toml`; a dependency carrying a different licence needs a lead decision
before it can land.

## SBOM contents

CycloneDX 1.5 JSON: Rust crates from `cargo metadata` (workspace crates as
`application` components, third-party crates as `library` components, SPDX
licence expression and the SHA-256 checksum from `Cargo.lock`), NuGet packages
from every committed `packages.lock.json` (SHA-512 content hash, as recorded
there; project-to-project entries are our own code and are dropped). The output
is sorted and contains no timestamp unless `--timestamp` is passed, so two runs
of one commit are byte-identical.