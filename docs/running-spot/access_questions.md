# Access questions (`[services.access_questions]`)

Provides the port `AccessQuestions` (what can this user do and why, who can do this, what changed between
two dates) over `spot-iam-questions` and the live directory.

| Setting | Default | Meaning |
| --- | --- | --- |
| `enabled` | true | Off: no port is provided. |
| `max_range_days` | 366 | The widest range `access_changes` accepts (1 to 3660). |

- The model is built for every question from one read-only transaction: stored roles, the duty definitions
  of the installed apps (`<app>/duties.toml`, `[[duty]]` tables), the active assignments, portal users, the
  effective metadata and the SoD rules (`<app>/sod_rules.toml`). Without duty definitions, or with a file that
  does not parse, every question answers unavailable (fail closed) and readiness is degraded.
- The asker is the authenticated user; the crate decides whether they may ask. Every question, refused ones
  included, writes a line to `spot::audit::access_questions` (asker, question, counts; never rows).
- Not yet: emergency access, the permission-change history (empty), documents and shares.
