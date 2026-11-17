# Graph service (`[services.graph]`)

The graph service answers `POST /api/v1/graph/query` for the signed-in user and keeps a per-site
in-memory index of the business graph. It is optional: a broken graph degrades readiness
(`graph`), the site keeps serving documents.

| Setting | Default | Meaning |
| --- | --- | --- |
| `enabled` | true | Off: the routes are not mounted. |
| `index_memory_mb` | 256 | Memory for the index; `0` turns it off (every query uses the database); otherwise 16 to 8192. |
| `graph_name` | `business` | The graph whose `sys_graph_edge` events this site applies. |
| `feed_interval_ms` | 500 | How often the outbox is read. |
| `drift_interval_secs` | 300 | How often the index is compared with `spot_graph_edge`; a mismatch rebuilds. |
| `max_nodes` | 10000 | Walk budget (1,000 to 100,000); a larger walk is `422 graph.too_large`. |
| `query_budget_ms` | 5000 | Wall time of one query. |

Behaviour: the index is built from the edge table off to the side (queries use the database
meanwhile) and kept current from the outbox. A decode error, gap or apply error marks it stale and
rebuilds it. Queries that follow relation edges always use the database. Each node has its own
index; there is no lease. Edge-type names on the wire are the canonical names with `.` replaced by `_`.

Audit: target `spot::audit::graph` (who, counts, refusal code; never values or record ids).
