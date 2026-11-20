Part of [Running Spot](../running-spot.md).

# Lineage service (`[services.lineage]`)

Serves `POST /api/v1/lineage` (`chain`, `trace_forward`, `trace_back`, `bom`, `where_used`) over the
hosted graph (`[services.graph]` must be enabled), as the requesting user.

Settings: `enabled` (true), `max_nodes` (5,000; 1 to 5,000; the ceiling of a request's `limit`).

Chain configuration: an installed app ships `lineage.toml` (`[[chain]]` with `doctype` and `edges`,
`[[bom]]` with `doctype`, `edge`, `qty_field`; edge names are canonical `doctype.field`). Files of all
apps are read together at start. No file: every kind answers `422 lineage.kind_unknown`. A file that
does not parse degrades readiness (`lineage`) and serves nothing. Quantities are exact decimal strings.
Audit target `spot::audit::lineage`: who, kind, counts, refusal codes.
