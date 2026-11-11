Part of [Running Spot](../running-spot.md).

# Print

`GET /api/v1/print/{doctype}/{id}` renders one document to a PDF **as the caller**: read
permission and field rights apply, and a missing document, one the caller may not read and an
unknown DocType are the same `404`. A DocType's print formats are template files in
`sites/<site>/templates/`: `<doctype>.typ` is the default format, `<doctype>.<format>.typ` the
format `?format=<format>`; a named format with no file is `print.format_unknown`. Without a
`<doctype>.typ` the built-in layout prints the title and every field that has a value.
Templates are Typst markup with MiniJinja tags; every value is escaped into a Typst string, and
money stays an exact decimal. The context is `doctype`, `id`, `title`, `printed_at`, `doc`
(typed fields), `tables` (child rows) and `fields` (label and text). Job and report code print
through the same service as the platform with `Printing::render_as_platform`.
