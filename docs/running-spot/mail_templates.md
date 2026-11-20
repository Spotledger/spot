Part of [Running Spot](../running-spot.md).

# E-mail templates

An app or a site operator writes an e-mail once, as a template. A sender renders it for one
recipient with a context and gets a subject, a text body and an HTML body. The library is
`spot-mail-render`; `spot mail-preview` shows the result without sending anything.

## Layout

A template is a folder `mail/<name>/` inside a template root:

| File | Holds |
| --- | --- |
| `subject.txt` | the subject, one line (required) |
| `body.txt` | the plain-text body |
| `body.html` | the HTML body |
| `subject.<locale>.txt`, `body.<locale>.txt`, `body.<locale>.html` | the same, for one locale (`ur-PK`, `en`) |

A template needs a subject and at least one body. `<name>` is lower-case letters, digits and
underscores (`invoice_sent`). A locale is a tag such as `en` or `ur-PK`.

## Where templates live, and who wins

Roots are searched in this order, and **the first root that has a folder of that name wins**
(the whole template, not file by file):

1. the site's template folder: `sites/<site>/templates` (the same folder print formats use;
   `services.print.template_root` renames it);
2. each installed app's `templates` folder, in the order of `common.toml` `apps`.

An app ships its templates as `<app>/templates/mail/<name>/...`. A site overrides one by
creating `sites/<site>/templates/mail/<name>/` with the files it wants: nothing of the app's
template is used then, so copy every file the site still needs.

A template folder, a name or a file that is a link, is not a regular file, is not UTF-8 or is
over 256 KiB is refused. Names with a path separator or a parent step are refused before any
file is read.

## Locale fallback

Each part (subject, text body, HTML body) is looked up on its own: the requested locale, then
`en`, then the file without a locale. A template can therefore have an Urdu HTML body and an
English-only text body. The result says which locale was used.

## What the template sees

The context is what the sender passes. Strings are escaped for the HTML body and left alone in
the subject and the text body. A variable the context lacks is an error that names the template,
file, line and variable; nothing renders with a blank.

- Amounts are exact decimals and are **formatted, never computed**: `{{ total | money('PKR') }}`,
  `{{ n | number(2) }}`, `{{ total | amount_in_words('PKR') }}`. Arithmetic on an amount is
  refused (`mail_render.not_computable`). Compute the amount in the sender and pass the result.
- Dates and times: `{{ due | date(format='medium') }}`, `{{ at | datetime(format='medium',
  tz='+05:00') }}`.
- Loops and conditions: `{% for l in lines %}...{% endfor %}`, `{% if x %}...{% endif %}`.

The subject must be one line of at most 255 characters; a value with a line break in it is
refused (header injection). Bodies are limited to 1 MiB. A template never chooses a sender, a
recipient or a header: the caller passes the sender and the recipients when it makes the message.
If only an HTML body exists, a plain-text alternative is derived by a fixed rule (script and style
removed, block ends become line breaks, cells tab-separated, other tags dropped, entities
decoded); if only text exists there is no HTML part.

Errors carry the template name, file and line only. They never repeat template source, a context
value or a recipient.

## Worked example

`apps/<app>/templates/mail/invoice/subject.txt`

```
Invoice {{ number }} from {{ company }}
```

`apps/<app>/templates/mail/invoice/body.html`

```
<p>Dear {{ customer }},</p><p>Invoice {{ number }} for {{ total | money('PKR') }} is due on {{ due | date(format='medium') }}.</p>
```

`apps/<app>/templates/mail/invoice/body.ur-PK.html` is the Urdu version, with `dir="rtl"`.

Preview it:

```
spot mail-preview invoice --site acme --locale ur-PK --context ctx.json
```

```json
{
  "customer": "Khan & Sons",
  "number": "INV-001",
  "company": "Acme",
  "total": {"$decimal": "12345.50"},
  "due": {"$date": "2026-03-10"}
}
```

In the context file an amount, a date and a time are one-key objects: `{"$decimal": "12.50"}`,
`{"$date": "2026-03-10"}`, `{"$datetime": "2026-03-10T14:45:00+05:00"}`. Any other number must be
an integer.

## `spot mail-preview`

```
spot mail-preview NAME --site SITE [--locale LOCALE] [--context FILE|-] [--format text|html|json]
```

It renders and prints; it sends nothing, opens no mail connection and needs no database. `text`
(the default) prints `Subject: ...`, a blank line and the text body (derived from the HTML body
when the template has none); `html` prints the HTML body; `json` prints the template, locale,
subject and both bodies. `--context -` reads standard input. Exit codes: 0 done, 1 usage, 3 failed
(the site, the template or the context could not be used; the message names the error code,
template, file and line).

## Using it from code

```rust
let renderer = MailRenderer::new(TemplateRoots::new(roots));
let rendered = renderer.render("invoice", Some("ur-PK"), &context)?;
let message = rendered.into_message(from, vec![to])?; // an OutgoingMessage for the queue
```

Where a sender gets the context for a document (a notification feature) is decided by that
feature; the seam is the `Context` argument: build it from the document's typed fields, with
amounts as decimals.
