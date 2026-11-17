# Rule graph (`[services.rule_graph]`)

Hosts `spot-rule-graph`: business rules as Datalog over the graph. Rule sets are
`<app>/rules/<name>.rules` of every installed app, compiled against the served metadata at start and
recompiled (by content hash) on every metadata swap. A set that fails keeps the previous one serving and
degrades readiness (`rule_graph`) naming the set and a stable code only.

Evaluation runs as the principal: only edges whose both ends the principal may read are facts. A program
that reads a field predicate is refused (`rule_graph.unsupported_field_fact`). Settings: `enabled`,
`max_facts` (100000), `fuel` (1000000), `max_rows` (10000), `budget_ms` (5000); a breach is
`rule_graph.limit`. The approver resolver takes a rule `"<set>/<predicate>"` and runs as
`form.requested_by`; no principal, no rule, a failure or no row resolves to nobody (the task creation fails
closed). Audit target `spot::audit::rule_graph`.
