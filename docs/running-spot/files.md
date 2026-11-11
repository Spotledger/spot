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
