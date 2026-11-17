# The migration audit: `spot migrate-audit`

Every lifecycle verb leaves a run record in the site's tamper-evident chain (`migration:<site>`):
the plan digest and summary, the approvals, the rehearsal report digest, the apply journal digest,
timings and the outcome (`planned`, `rehearsed`, `applied`, `verified`, `failed`, `rolled_back`,
`refused`). A refused or failed verb is recorded too. The record holds ids, digests, counts and
outcomes, never a row value or a secret.

```
spot migrate-audit list   --site S [--plan HASH] [--since YYYY-MM-DD] [--json]
spot migrate-audit show   RUN_ID --site S [--explain]
spot migrate-audit verify --site S
```

* `list` and `show` need the `migration.run` right (`migration.not_permitted` otherwise) and only
  ever read the site's own chain: the runs of site A are invisible from site B.
* `show --explain` says in sentences what changed, who approved, what the rehearsal found and what
  happened.
* `verify` re-walks every hash and link and prints the first broken sequence; exit 2 when the chain
  has a break, 0 when it is intact.

For developers: `spot_migration_ops::audit::record_verb` wraps a verb's work: the run begins before
the work (a run that cannot be recorded does not start the work) and finishes with the outcome,
`refused` for a policy or permission refusal and `failed` for a failure.
