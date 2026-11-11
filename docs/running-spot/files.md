Part of [Running Spot](../running-spot.md).

# Files

```
<root>/common.toml                 host configuration (below)
<root>/sites/<site>/site.toml      the site: database, apps, key digests, password verifiers
<root>/sites/<site>/metadata.json  runtime metadata changes (custom fields), replayed at start
<root>/sites/<site>/reload         touch to rebuild the site runtime (`spot reload`)
<root>/sites/<site>/files/         uploaded bytes, when services.files.provider = "local"
<root>/sites/<site>/templates/     print templates, when services.print is enabled
```

`site.toml`, the credentials in it and `services.mail.smtp.password` are secrets: they are
never logged, never put in a readiness detail, and never echoed in an error. Keep the root
readable only by the service account.

## Attached files

An uploaded file starts `pending` and is readable by its uploader only. When a document is saved
with an `attach` field naming it, the host marks the file `attached` to that document and field
right after the commit; from then on whoever may read the document (and the field) may download
the file, and the uploader needs no right of their own. Replacing or clearing the value, or
deleting the document, marks the file `orphaned`, and the orphan sweep deletes its bytes after
its retention. Only the uploader (or the platform itself) can attach a file: a save that names
another user's unattached file, a file attached to another document, or an unknown id attaches
nothing. A failed attach step is logged and never fails the save. The host's committed steps run
in the saving request, so a crash between the commit and the step would leave the file `pending`;
the next `spot serve` start re-applies the step for every document that names a file still
`pending` whose uploader can read that document.
