Part of [Running Spot](../running-spot.md).

# File uploads

Uploads and downloads go through `/api/v1/files`. The bytes live in the provider named by
`services.files.provider`: `local` (the default) keeps them under `sites/<site>/files/`, in
keys the server makes (never from the client's name). The file records are documents of the
platform `file` DocType, so they survive a restart and appear in the audit. An unattached file
can be read only by whoever uploaded it; an attached file by whoever may read its document (and
field). Every refusal, an unknown id included, is the same `404 document.not_found`.

A file's key under `files/` is a function of its id and creation time only
(`YYYY/MM/<id>`), recomputed whenever a record is read; the record stores no key and attaching a
file (which makes it readable by whoever may read the document) never moves its bytes. A range
request (`Range: bytes=a-b`) answers `206` for whoever may read the file, before and after the
attach, and after a restart.
