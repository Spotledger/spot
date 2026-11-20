Part of [Running Spot](../running-spot.md).

# The field guard on a running server

`spot serve` and `spot console` put the field guard in front of every channel that can carry a
field value or a field name out of the server. A field in a protection group (or with a mask) is
shown to the caller only as the permission engine allows; the guard is the one place that decides.

## Which channels are guarded

| Channel | What the guard does | Installed by |
|---|---|---|
| List and query (`GET /api/v1/resource/{doctype}`, `POST .../query`) | A column the caller may not report is removed; a masked column is masked. A filter or sort on a field the caller may not report is answered as `document.unknown_field`, exactly like a field that does not exist. | `LiveResources` (`live.rs`), `spot console` |
| Link lookup (`POST /api/v1/lookup/{doctype}/{field}`) | A label is built only from fields the caller may read (masked when the caller may only read the masked form); otherwise the row is labelled by its id. Text typed against a protected title matches nothing, so the answer is the answer to any plain miss. | `LiveLookup` (`live.rs`), `spot console` |
| Realtime rooms and notices | A protected field name never appears in a payload shared by a room, and a notice that carries a value of a protected field is refused. A client that needs a value fetches the document through the normal read path, where each field is checked for that caller. | `FeedPump::with_notification_guard` (`services/realtime.rs`) |

The guard reads the metadata the site serves **now**. A field protected by a customisation that is
swapped in while the server runs is protected from the next request or event; no restart is
needed. The decisions are the permission engine's own: the guard adds no rule of its own.

## Fail closed

- The guard is built per request from the metadata served at that moment. When nothing can be
  served (the node is diverged from its peers, or no runtime is attached) the list and the lookup
  answer `server.unavailable`. No unguarded page and no unguarded label is ever returned.
- There is no unguarded form of the lookup: `EngineLookup` requires its guard.
- For realtime, a DocType the guard does not know releases no field name at all.
- A full audit queue withholds the value of a trace-group field (the guard's own default).

## Search and `index_protected`

Search is guarded where it is hosted (`Indexer::with_protection`, `Retriever::with_search_guard`).
`spot-cli` hosts no retriever or indexer yet, so nothing is wired here and the setting
`[services.field_guard] index_protected` is not read by the server today. When a host for search
is added it must install both guards and read that setting, a boolean that is **off** by default:
a protected field is then left out of the index.

## Known gap

The list path applies no row-level `PermissionFilter`; only the per-row load is authorised (see the
report of WP-SEC-04.T5). The field guard therefore protects columns and field names, not which
rows appear in a page.
