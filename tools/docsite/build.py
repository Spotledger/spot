#!/usr/bin/env python3
"""Build the Spotledger contract documentation site (WP-DX-04.T1).

Generates a deterministic, self-contained static site — plain HTML and CSS,
no script and no CDN, viewable offline from ``file://`` — from the contracts
checked into the repository:

- ``contracts/native-api/openapi.yaml`` → the Native API pages: one page per
  resource tag with every operation's parameters, request/response schemas,
  examples and per-operation error codes; one page per component schema;
  the shared components; and a top-level ``x-spot-error-codes`` index.
- ``contracts/native-api/realtime.md`` → the realtime page, with the message
  schemas from the OpenAPI rendered under their client/server sections.
- ``docs/adr/ADR-053…ADR-057.md``, ``contracts/*/CHANGELOG.md`` and the rust
  rustdoc trees of ``spot-app-api``, ``spot-storage-contract`` and
  ``spot-events-contract`` (built into ``target/doc``), plus one overview
  page per contract.

Every page carries the shared sidebar (Native API, Realtime, Events, App API,
Storage, ADRs, Error codes). ``--check`` builds the same site and then
verifies — without weakening any gate — that every internal link and anchor
resolves, every operation got its ``op-<operationId>`` anchor, every schema
its ``schema-<Name>`` anchor, every message type its ``msg-<type>`` anchor,
and every error code named anywhere in the OpenAPI is documented in the
index, rejecting unknown envelope codes and broken ``$ref``s.

No path under ``contracts/`` is written by this tool; those files are inputs.

Run ``python tools/docsite/build.py --out DIR [--check]`` from any directory:
``--out`` receives the whole site (a marker file lets rebuilds wipe it),
``--check`` fails on any problem with a message naming it.
"""
from __future__ import annotations

import argparse
import json
import posixpath
import re
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bootstrap

bootstrap.ensure_deps()  # installs + re-execs the command when pins are missing
import yaml  # noqa: E402  (imports after the bootstrap, on purpose)
from jinja2 import Environment, FileSystemLoader, StrictUndefined  # noqa: E402
from markdown_it import MarkdownIt  # noqa: E402

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
MARKER = ".spot-docsite"  # inside --out: marks it as ours, so rebuilds may wipe it

METHODS = ("get", "put", "post", "delete", "patch", "head", "options", "trace")
KEYS = ("native-api", "app-api", "events", "storage")
CRATE_OF = {"app-api": "spot_app_api", "events": "spot_events_contract",
            "storage": "spot_storage_contract"}
ADR_OF = {"native-api": "ADR-055-native-api-contract-v0.1",
          "app-api": "ADR-057-app-api-contract-v0.1",
          "events": "ADR-056-event-and-outbox-schema-v0.1",
          "storage": "ADR-053-storage-contract-v0.1"}
BLURB = {
    "native-api": "The only way clients reach the runtime: the OpenAPI 3.1 HTTP "
                  "contract and the realtime WebSocket protocol (ADR-055).",
    "app-api": "The one crate app code may depend on: the dynamic document handle, "
               "typed views, lifecycle hooks, controllers, typed errors (ADR-057).",
    "events": "Event and outbox schema: topic and payload metadata, the external "
              "envelope, payload schemas and the compatibility checker (ADR-056).",
    "storage": "The backend contract: traits, documents with child rows, the typed "
               "query IR, locks, sequences, outbox and change feed (ADR-053).",
}
CODE_RE = re.compile(r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$")

def esc(text: object) -> str:
    import html as _html
    return _html.escape(str(text), quote=True)


_MD = MarkdownIt("commonmark").enable("table")


def md_inline(text: str) -> str:
    return _MD.renderInline(text.strip()) if text.strip() else ""


def md_block(text: str) -> str:
    return add_heading_ids(_MD.render(text))


def anchor_slug(html_text: str) -> str:
    text = re.sub(r"<[^>]+>", "", html_text)
    text = text.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
    return re.sub(r"[^a-z0-9]+", "-", text.strip().lower()).strip("-") or "section"


def add_heading_ids(html_text: str) -> str:
    """Give rendered markdown headings deterministic slug ids (deduplicated)."""
    seen: set[str] = set()

    def _fix(match: re.Match[str]) -> str:
        level, inner = match.group(1), match.group(2)
        base = anchor_slug(inner)
        slug, n = base, 2
        while slug in seen:
            slug = f"{base}-{n}"
            n += 1
        seen.add(slug)
        return f'<h{level} id="{slug}">{inner}</h{level}>'

    return re.sub(r"<h([1-6])>(.*?)</h\1>", _fix, html_text, flags=re.S)


def json_code(value: object) -> str:
    return esc(json.dumps(value, indent=2, ensure_ascii=False))


class Site:
    """Generated pages plus every problem found while building them."""

    def __init__(self, root: Path, jinja: Environment) -> None:
        self.root = root
        self.jinja = jinja
        self.pages: dict[str, str] = {}
        self.problems: list[str] = []
        self.required: dict[str, list[str]] = {"op": [], "schema": [], "code": [], "msg": []}

    def fail(self, message: str) -> None:
        self.problems.append(message)

    def add(self, path: str, title: str, content: str, nav: list[dict],
            commit: str | None) -> None:
        """Render one page through the shared template at its repo-site path."""
        prefix = "../" * path.count("/")
        entries = []
        for group in nav:
            entries.append({"label": group[0], "pages": [
                {"name": name, "href": prefix + href, "active": href == path}
                for href, name in group[1]
            ]})
        self.pages[path] = self.jinja.get_template("page.html").render(
            title=title, prefix=prefix, content=content, nav=entries, commit=commit)


def resolve_ref(spec: dict, ref: object, site: Site, where: str) -> object | None:
    """Resolve a local ``#/...`` JSON pointer; name any dead one as a problem."""
    if not isinstance(ref, str) or not ref.startswith("#/"):
        return None
    node: object = spec
    for part in ref[2:].split("/"):
        part = part.replace("~1", "/").replace("~0", "~")
        if not isinstance(node, dict) or part not in node:
            site.fail(f"broken $ref {ref!r} at {where}")
            return None
        node = node[part]
    return node


def walk_spec(node: object, path: tuple[object, ...]) -> list[tuple[dict, tuple[object, ...]]]:
    found: list[tuple[dict, tuple[object, ...]]] = []
    if isinstance(node, dict):
        found.append((node, path))
        for key, value in node.items():
            found += walk_spec(value, (*path, key))
    elif isinstance(node, list):
        for i, item in enumerate(node):
            found += walk_spec(item, (*path, i))
    return found


def _envelope_code(example: object) -> str | None:
    if isinstance(example, dict) and isinstance(example.get("error"), dict) \
            and isinstance(example["error"].get("code"), str):
        return example["error"]["code"]
    return None


def used_code_nodes(spec: dict) -> list[tuple[tuple[object, ...], object]]:
    """(pointer, code) for every envelope error code named in an example,
    mirroring the walk in the contract's check.py: media type objects
    (``example`` / ``examples`` maps, where each named example's ``value``
    carries the payload) and component schemas with ``examples`` lists."""
    out = []
    for node, path in walk_spec(spec, ()):
        if not isinstance(node, dict):
            continue
        if "schema" in node and isinstance(node["schema"], dict):
            if "example" in node:
                code = _envelope_code(node["example"])
                if code:
                    out.append((path, code))
            examples = node.get("examples")
            if isinstance(examples, dict):
                for example in examples.values():
                    if isinstance(example, dict) and "value" in example:
                        code = _envelope_code(example["value"])
                        if code:
                            out.append((path, code))
        if len(path) == 3 and path[:2] == ("components", "schemas") \
                and isinstance(node.get("examples"), list):
            for example in node["examples"]:
                code = _envelope_code(example)
                if code:
                    out.append((path + ("examples",), code))
    return out

class Gen:
    """Renders every page's HTML fragment from the loaded spec and inputs."""

    def __init__(self, site: Site, spec: dict, root: Path, rust: set[str],
                 contracts: tuple[str, ...], adrs: list[Path], commit: str | None,
                 versions: dict[str, str] | None = None,
                 changelogs: set[str] | None = None) -> None:
        self.site = site
        self.spec = spec
        self.root = root
        self.rust = rust
        self.contracts = contracts
        self.adrs = adrs
        self.commit = commit
        self.versions = versions or {}
        self.changelogs = changelogs or set()
        self.page = ""  # repo-site path of the page currently being rendered

    # -- link helpers --------------------------------------------------------
    def at(self, target: str, fragment: str = "") -> str:
        """URL of another generated page, from the page being rendered."""
        prefix = "../" * self.page.count("/")
        return f"{prefix}{target}{fragment}"

    def link(self, target: str, fragment: str, body: str) -> str:
        return f'<a href="{self.at(target, fragment)}">{body}</a>'

    def schema_link(self, name: str) -> str:
        return self.link(f"native/schemas/{name}.html", f"#schema-{name}",
                         f"<code>{esc(name)}</code>")

    def code_link(self, code: str) -> str:
        return self.link("errors.html", f"#code-{code}", f"<code>{esc(code)}</code>")

    def rust_links(self, key: str) -> str:
        if key not in ("app-api", "events", "storage") or CRATE_OF.get(key) not in self.rust:
            return ""
        crate = CRATE_OF[key]
        return f"<li>rustdoc: {self.link(f'rust/{crate}/index.html', '', crate)}</li>"

    # -- schema fragments ------------------------------------------------------
    def type_html(self, schema: object) -> str:
        if isinstance(schema, dict) and "$ref" in schema:
            name = str(schema["$ref"]).rsplit("/", 1)[-1]
            target = resolve_ref(self.spec, schema["$ref"], self.site, f"schemas/{name}")
            if target is None:
                return f'<span class="warn"><code>{esc(schema["$ref"])}</code></span>'
            return self.schema_link(name)
        if isinstance(schema, dict):
            if "const" in schema:
                return f"<code>{json_code(schema['const'])}</code>"
            if "enum" in schema:
                return " ".join(f"<code>{json_code(v)}</code>" for v in schema["enum"])
            types = schema.get("type")
            if isinstance(types, list):
                inner = " | ".join(self.type_html({**schema, "type": t}) for t in types
                                   if t != "null")
                return f"{inner} | <code>null</code>" if inner else "<code>null</code>"
            if types == "array":
                return f"[{self.type_html(schema.get('items', {}))}]"
            if types == "object":
                return "<code>object</code>"
            if types:
                return f"<code>{esc(types)}</code>"
            if "oneOf" in schema or "anyOf" in schema:
                return "<code>(one of the variants below)</code>"
        return "<code>—</code>"

    def chips(self, schema: object) -> str:
        if not isinstance(schema, dict):
            return ""
        labels = [("format", "format"), ("pattern", "pattern"), ("minimum", "min"),
                  ("maximum", "max"), ("minLength", "minLength"), ("maxLength", "maxLength"),
                  ("minItems", "minItems"), ("maxItems", "maxItems"),
                  ("minProperties", "minProps"), ("maxProperties", "maxProps")]
        parts = [f'<span class="chip">{esc(label)} {json_code(schema[key])}</span>'
                 for key, label in labels if key in schema and key != "$ref"]
        if schema.get("default") is not None:
            parts.append(f'<span class="chip">default {json_code(schema["default"])}</span>')
        if "additionalProperties" in schema and schema["additionalProperties"] is not True:
            inner = schema["additionalProperties"]
            body = self.type_html(inner) if isinstance(inner, dict) else "<code>false</code>"
            parts.append(f'<span class="chip">additionalProperties {body}</span>')
        return "".join(f'<p class="meta">{p}</p>' for p in parts)

    def props_table(self, schema: dict) -> str:
        props = schema.get("properties")
        if not isinstance(props, dict) or not props:
            return ""
        required = schema.get("required") or []
        rows = []
        for name, prop in props.items():
            chips = self.chips(prop)
            rows.append(
                f"<tr><td><code>{esc(name)}</code></td>"
                f"<td>{'✓' if name in required else ''}</td>"
                f"<td>{self.type_html(prop)}</td>"
                f"<td>{md_inline(str(prop.get('description', '')))}{chips}</td></tr>")
        return ('<table><thead><tr><th>Property</th><th>Required</th><th>Type</th>'
                '<th>Description</th></tr></thead><tbody>' + "".join(rows)
                + '</tbody></table>')

    def examples_html(self, node: dict) -> str:
        parts = []
        if "example" in node:
            head = self.error_chips(node["example"])
            parts.append(f"{head}<pre><code>{json_code(node['example'])}</code></pre>")
        for name, example in sorted((node.get("examples") or {}).items()):
            if isinstance(example, dict) and "value" in example:
                chips = self.error_chips(example["value"])
                parts.append(f'<div class="response"><h4><code>{esc(name)}</code>'
                             f'{chips}</h4><pre><code>{json_code(example["value"])}'
                             "</code></pre></div>")
        return "".join(parts)

    def error_chips(self, payload: object) -> str:
        code = _envelope_code(payload)
        if not code or not CODE_RE.match(code):
            return ""
        return f' <span class="chip">{self.code_link(code)}</span>'

    def schema_body(self, name: str, schema: dict, msg_links: bool = False) -> str:
        """Full rendering of one (sub)schema: description, properties, variants."""
        parts = []
        if schema.get("required"):
            parts.append('<p class="meta">required: '
                         + ", ".join(f"<code>{esc(r)}</code>"
                                     for r in schema["required"]) + "</p>")
        parts.append(f'<div>{md_block(str(schema.get("description", "")))}</div>')
        parts.append(self.props_table(schema))
        variants = schema.get("oneOf") or schema.get("anyOf") or []
        for i, variant in enumerate(variants):
            const = (variant.get("properties") or {}).get("type", {}).get("const")
            if const and msg_links:
                head = (f'<section class="msg" id="msg-{esc(const)}">'
                        f"<h3><code>{esc(const)}</code> message</h3>")
                self.site.required["msg"].append(f"msg-{const}")
            else:
                head = '<section class="variant"><h3>Variant ' + str(i + 1) + "</h3>"
            body = self.props_table(variant) if variant.get("properties") \
                else f"<div>{self.type_html(variant)}</div>"
            parts.append(head + body + self.chips(variant) + "</section>")
        for extra in ("propertyNames", "not"):
            if extra in schema:
                parts.append(f'<p class="meta">validated with <code>{esc(extra)}</code> '
                             f'<span class="chip">{json_code(schema[extra])}</span></p>')
        return "".join(parts)

    # -- operation fragments ---------------------------------------------------
    def merged_params(self, path_item: dict, op: dict) -> list[dict]:
        merged: list[dict] = []
        for raw in [*(path_item.get("parameters") or []), *(op.get("parameters") or [])]:
            resolved = resolve_ref(self.spec, raw.get("$ref"), self.site,
                                   "parameters") if isinstance(raw, dict) \
                and "$ref" in raw else raw or {}
            if isinstance(resolved, dict) and resolved.get("name"):
                if not any(p.get("in") == resolved.get("in")
                           and p.get("name") == resolved.get("name") for p in merged):
                    merged.append(resolved)
        return merged

    def params_table(self, params: list[dict]) -> str:
        if not params:
            return ""
        rows = []
        for param in params:
            schema = param.get("schema") or {}
            if isinstance(schema, dict) and "$ref" in schema:
                schema = resolve_ref(self.spec, schema["$ref"], self.site,
                                     f"parameters/{param.get('name')}")
            body = self.type_html(schema or {}) + self.chips(schema)
            rows.append(f"<tr><td><code>{esc(param.get('name'))}</code></td>"
                        f"<td>{esc(param.get('in', ''))}"
                        f"{'<br>✓' if param.get('required') else ''}</td>"
                        f"<td>{body}</td>"
                        f"<td>{md_inline(str(param.get('description', '')))}</td></tr>")
        return ('<table><thead><tr><th>Parameter</th><th>In</th><th>Type</th>'
                "<th>Description</th></tr></thead><tbody>" + "".join(rows) + "</tbody></table>")

    def body_html(self, body: object, where: str) -> str:
        if not isinstance(body, dict):
            return ""
        parts = [f'<p class="meta">required</p>'] if body.get("required") else []
        if body.get("description"):
            parts.append(f'<div>{md_block(str(body["description"]))}</div>')
        for media, content in (body.get("content") or {}).items():
            if not isinstance(content, dict):
                continue
            parts.append(f'<p class="meta"><code>{esc(media)}</code> → '
                         f"{self.type_html(content.get('schema'))}</p>")
            parts.append(self.examples_html(content))
        return "".join(parts)

    def responses_html(self, op: dict) -> str:
        parts = []
        for status, resp in (op.get("responses") or {}).items():
            if isinstance(resp, dict) and "$ref" in resp:
                name = str(resp["$ref"]).rsplit("/", 1)[-1]
                if resolve_ref(self.spec, resp["$ref"], self.site,
                               f"responses/{name}") is not None:
                    parts.append('<div class="response"><h3><code>'
                                 + esc(status) + "</code> — "
                                 + self.link("native/index.html", f"#resp-{name}",
                                             f"shared response <code>{esc(name)}</code>")
                                 + "</h3></div>")
                continue
            resp = resp or {}
            parts.append(f'<div class="response"><h3><code>{esc(status)}</code> '
                         f"{md_inline(str(resp.get('description', '')))}</h3>")
            for name, header in sorted((resp.get("headers") or {}).items()):
                if isinstance(header, dict) and "$ref" in header:
                    hname = str(header["$ref"]).rsplit("/", 1)[-1]
                    if resolve_ref(self.spec, header["$ref"], self.site,
                                   f"headers/{hname}") is not None:
                        parts.append('<p class="meta">header ' + self.link(
                            "native/index.html", f"#hdr-{hname}",
                            f"<code>{esc(name)}</code>") + "</p>")
                    continue
                parts.append(f'<p class="meta">header <code>{esc(name)}</code> — '
                             + md_inline(str(header.get("description", ""))) + "</p>")
            for media, content in (resp.get("content") or {}).items():
                parts.append(f'<p class="meta"><code>{esc(media)}</code> → '
                             f"{self.type_html(content.get('schema'))}</p>")
                parts.append(self.examples_html(content))
            parts.append("</div>")
        return "".join(parts)

    def op_section(self, route: str, method: str, op: dict, path_item: dict) -> str:
        op_id = op.get("operationId") or f"{method}_{route}"
        self.site.required["op"].append(f"op-{op_id}")
        head = (f'<section class="op" id="op-{esc(op_id)}"><h2>'
                f'<span class="method {esc(method)}">{esc(method.upper())}</span> '
                f'<span class="route"><code>{esc(route)}</code></span></h2>')
        parts = [head]
        if op.get("summary"):
            parts.append(f"<p>{md_inline(str(op['summary']))}</p>")
        if op.get("description"):
            parts.append(f'<div>{md_block(str(op["description"]))}</div>')
        parts.append(self.params_table(self.merged_params(path_item, op)))
        if "requestBody" in op:
            parts.append("<h3>Request body</h3>")
            parts.append(self.body_html(op["requestBody"], op_id))
        parts.append("<h3>Responses</h3>")
        parts.append(self.responses_html(op))
        parts.append("</section>")
        return "".join(parts)

    # -- pages -------------------------------------------------------------------
    def ops_by_tag(self) -> list[tuple[str, list[tuple[str, str, dict, dict]]]]:
        """(tag, [(route, method, op, path_item)]) in spec order (page-ready)."""
        declared = [t.get("name", "") for t in self.spec.get("tags") or []]
        buckets: dict[str, list] = {tag: [] for tag in declared}
        for route, path_item in (self.spec.get("paths") or {}).items():
            if not isinstance(path_item, dict):
                continue
            for method in METHODS:
                if not isinstance(path_item.get(method), dict):
                    continue
                op = path_item[method]
                tag = (op.get("tags") or [declared[0] if declared else "other"])[0]
                buckets.setdefault(tag, []).append((str(route), method, op, path_item))
        order = declared + sorted(t for t in buckets if t not in declared)
        return [(tag, buckets[tag]) for tag in order if buckets.get(tag)]

    def page_native(self) -> None:
        self.page = "native/index.html"
        info = self.spec.get("info") or {}
        links = [f'<span class="badge">v{esc(info.get("version", "?"))}</span>']
        if "native-api" in self.changelogs:
            links.append('<a href="' + self.at("changelog/native-api.html")
                         + '">changelog</a>')
        parts = [f'<h1>{esc(info.get("title", "Native API"))}</h1>'
                 f"<p>{' · '.join(links)} OpenAPI 3.1</p>"]
        if info.get("description"):
            parts.append(md_block(str(info["description"])))
        parts.append('<h2 id="servers">Servers and auth</h2>')
        for server in self.spec.get("servers") or []:
            parts.append("<p><code>" + esc(server.get("url", "")) + "</code> — "
                         + md_inline(str(server.get("description", ""))) + "</p>")
        for name, scheme in (self.spec.get("components", {}).get("securitySchemes")
                             or {}).items():
            parts.append(f"<p><strong>{esc(name)}</strong> — "
                         + md_inline(str(scheme.get("description", "")))
                         + f' (type <code>{esc(scheme.get("type"))}</code>)</p>')
        parts.append('<h2 id="tags">Resources</h2><ul>')
        for tag, _ in self.ops_by_tag():
            parts.append("<li><a href=\""
                         + self.at(f"native/tag-{tag}.html") + f'">{esc(tag)}</a></li>')
        parts.append("<li><a href=\""
                     + self.at("native/schemas/index.html") + '">component schemas</a></li></ul>')
        comps = self.spec.get("components") or {}
        parts.append('<h2 id="shared">Shared parameters, headers and responses</h2>')
        parts.append(self.params_table(list((comps.get("parameters") or {}).values())))
        rows = "".join(f'<tr id="hdr-{esc(name)}"><td><code>{esc(name)}</code></td><td>'
                       + md_inline(str(h.get("description", ""))) + "</td><td>"
                       + self.type_html(h.get("schema")) + "</td></tr>"
                       for name, h in (comps.get("headers") or {}).items())
        if rows:
            parts.append('<table><thead><tr><th>Header</th><th>Description</th>'
                         f"<th>Type</th></tr></thead><tbody>{rows}</tbody></table>")
        for name, resp in (comps.get("responses") or {}).items():
            parts.append(f'<div class="response" id="resp-{esc(name)}">'
                         f"<h3><code>{esc(name)}</code> — "
                         + md_inline(str(resp.get("description", ""))) + "</h3>")
            for media, content in (resp.get("content") or {}).items():
                parts.append(f'<p class="meta"><code>{esc(media)}</code> → '
                             f"{self.type_html(content.get('schema'))}</p>")
                parts.append(self.examples_html(content))
            parts.append("</div>")
        parts.append('<h2>Schemas and error codes</h2><p>Every component schema has '
                     'its own page: see <a href="'
                     + self.at("native/schemas/index.html") + '">Schemas</a>. '
                     "Every error code is listed on the <a href=\""
                     + self.at("errors.html") + '">Error codes</a> page.</p>')
        self.site.add(self.page, "Native API", "".join(parts), self.nav(), self.commit)

    def page_tag(self, tag: str) -> None:
        self.page = f"native/tag-{tag}.html"
        parts = [f"<h1>Native API — {esc(tag)}</h1>", "<p>Also on this contract: <a href=\""
                 + self.at("native/index.html") + '">overview</a>, <a href="'
                 + self.at("native/schemas/index.html") + '">schemas</a>, <a href="'
                 + self.at("realtime.html") + '">realtime</a>, <a href="'
                 + self.at("errors.html") + '">error codes</a>.</p>']
        for bucket_tag, ops in self.ops_by_tag():
            if bucket_tag != tag:
                continue
            for route, method, op, path_item in ops:
                parts.append(self.op_section(route, method, op, path_item))
        self.site.add(self.page, f"Native API · {tag}", "".join(parts),
                      self.nav(), self.commit)

    def page_schemas(self) -> None:
        self.page = "native/schemas/index.html"
        parts = ["<h1>Component schemas</h1><p>Component schemas of <a href=\""
                 + self.at("native/index.html")
                 + '">Native API</a>.</p><table><thead><tr><th>Schema</th>'
                 "<th>Description</th></tr></thead><tbody>"]
        for name, schema in (self.spec.get("components", {}).get("schemas") or {}).items():
            desc = md_inline(str(schema.get("description", "")).split("\n\n")[0])
            parts.append("<tr><td>" + self.schema_link(name) + "</td>"
                         f"<td>{desc}</td></tr>")
            self.site.required["schema"].append(f"schema-{name}")
        parts.append("</tbody></table>")
        self.site.add(self.page, "Schemas", "".join(parts), self.nav(), self.commit)

    def page_schema(self, name: str) -> None:
        self.page = f"native/schemas/{name}.html"
        schema = (self.spec.get("components", {}).get("schemas") or {}).get(name) or {}
        parts = ['<p class="crumbs"><a href="'
                 + self.at("native/index.html") + '">Native API</a> › <a href="'
                 + self.at("native/schemas/index.html")
                 + f'">Schemas</a> › {esc(name)}</p>',
                 f'<h1 id="schema-{esc(name)}"><code>{esc(name)}</code></h1>',
                 self.schema_body(name, schema)]
        self.site.required["schema"].append(f"schema-{name}")
        self.site.add(self.page, f"Schema {name}", "".join(parts), self.nav(), self.commit)

    def page_realtime(self) -> None:
        """Realtime protocol page: the markdown doc, with the message schemas
        from the OpenAPI injected under their client/server message sections."""
        self.page = "realtime.html"
        md = (self.root / "contracts" / "native-api" / "realtime.md").read_text()
        content = md_block(md)
        schemas = (self.spec.get("components", {}).get("schemas") or {})
        injected = []  # schema keys that landed under their markdown section
        orphaned = []  # schemas whose section heading was missing
        for key, heading in (("RealtimeClientMessage", "client"),
                             ("RealtimeServerMessage", "server")):
            schema = schemas.get(key)
            if not isinstance(schema, dict) or not (schema.get("oneOf") or schema.get("anyOf")):
                continue
            block = f'<section class="msglist"><h3>{esc(key)} — message types</h3>' \
                    + self.schema_body(key, schema, msg_links=True) + "</section>"
            pattern = rf'(<h2 id="{heading}-messages">.*?</h2>)'
            if re.search(pattern, content):
                content = re.sub(pattern, lambda m, b=block: m.group(1) + b,
                                 content, count=1)
                injected.append(key)
            else:
                orphaned.append(block)
        for block in orphaned:  # heading missing in realtime.md; list them at the end
            content += block
        label = esc(", ".join(injected) or "no message type")
        badge = self.versions.get("native-api", "0.1")
        parts = ['<h1>Realtime protocol</h1>'
                 f'<p><span class="badge">v{esc(badge)}</span> '
                 '<a href="' + self.at("native/index.html") + '">Native API</a> · '
                 'rendered from <code>contracts/native-api/realtime.md</code>, with the '
                 f"<code>{label}</code> schemas injected from the "
                 "components of <code>openapi.yaml</code>.</p>", content]
        self.site.add(self.page, "Realtime protocol", "".join(parts), self.nav(), self.commit)

    def page_contract(self, key: str) -> None:
        """Overview page for app-api / events / storage (and the native API's
        own badge lives on native/index.html)."""
        self.page = f"{key}.html"
        names = {"app-api": "App API", "events": "Events", "storage": "Storage",
                 "native-api": "Native API"}
        parts = ["<p class=\"crumbs\"><a href=\"" + self.at("index.html") + '">Home</a></p>',
                 f'<h1>{names[key]}</h1>',
                 f"<p><span class=\"badge\">v{esc(self.versions.get(key, '?'))}</span></p>",
                 f"<p>{BLURB[key]}</p>", "<ul>",
                 f"<li>contract directory: <code>contracts/{esc(key)}</code></li>"]
        parts.append(self.rust_links(key))
        adr = ADR_OF.get(key)
        if adr and adr in {p.stem for p in self.adrs}:
            parts.append("<li>ADR: " + self.link(f"adr/{adr}.html", "", f"<code>{adr}</code>")
                         + "</li>")
        if key in self.changelogs:
            parts.append('<li>changelog: '
                         + self.link(f"changelog/{key}.html", "", "<code>CHANGELOG.md</code>")
                         + "</li>")
        parts.append("</ul>")
        self.site.add(self.page, names[key], "".join(parts), self.nav(), self.commit)

    def page_changelog(self, key: str) -> None:
        self.page = f"changelog/{key}.html"
        md = (self.root / "contracts" / key / "CHANGELOG.md").read_text()
        names = {"native-api": "Native API", "app-api": "App API",
                 "events": "Events", "storage": "Storage"}
        parts = ['<p class="crumbs"><a href="'
                 + self.at(f"{key}.html" if key != "native-api" else "native/index.html")
                 + f'">{names[key]}</a> › changelog</p>',
                 f"<h1>{names[key]} — contract changelog</h1>"
                 f'<p><span class="badge">v{esc(self.versions.get(key, "?"))}</span></p>',
                 md_block(md)]
        self.site.add(self.page, f"Changelog · {names[key]}", "".join(parts),
                      self.nav(), self.commit)

    def page_adr(self, path: Path) -> None:
        stem = path.stem
        self.page = f"adr/{stem}.html"
        md = path.read_text(errors="replace")
        title = re.search(r"^#\s+(.+)$", md, re.M)
        parts = ['<p class="crumbs"><a href="'
                 + self.at("adr/index.html") + '">Decision records</a></p>',
                 "<h1>" + esc(title.group(1) if title else stem) + "</h1>",
                 md_block(md)]
        self.site.add(self.page, f"ADR · {title.group(1) if title else stem}",
                      "".join(parts), self.nav(), self.commit)

    def page_adrs(self) -> None:
        self.page = "adr/index.html"
        parts = ["<h1>Architecture decision records</h1><p>The ADR-053…ADR-057 "
                 "records the contracts are built on, rendered verbatim from "
                 "<code>docs/adr/</code>.</p><ul>"]
        for path in self.adrs:
            title = re.search(r"^#\s+(.+)$", path.read_text(errors="replace"), re.M)
            link = self.link(f"adr/{path.stem}.html", "",
                             esc(title.group(1) if title else path.stem))
            parts.append(f"<li>{link}</li>")
        parts.append("</ul>")
        self.site.add(self.page, "Decision records", "".join(parts), self.nav(), self.commit)

    def page_errors(self, registry: dict, used: list[str]) -> None:
        """Error-code index: every registry row, plus any envelope code found in
        an example (unregistered ones are flagged, never silently dropped)."""
        self.page = "errors.html"
        for code in used:
            if code not in registry:
                self.site.fail(f"unregistered error code {code!r} named in an "
                               "example; add it to x-spot-error-codes")
        rows = []
        for code, (status, retryable, meaning) in registry.items():
            rows.append(f'<tr id="code-{esc(code)}"><td><code>{esc(code)}</code></td>'
                        f"<td>{esc(status)}</td><td>{'✓' if retryable else ''}</td>"
                        f"<td>{md_inline(str(meaning))}</td></tr>")
            self.site.required["code"].append(f"code-{code}")
        parts = ["<h1>Error codes</h1><p>Every code in the "
                 f"<code>x-spot-error-codes</code> registry of <a href=\""
                 + self.at("native/index.html") + '">Native API</a>, plus every code '
                 "an example names (each response example links back here).</p>"
                 '<table><thead><tr><th>Code</th><th>HTTP</th><th>Retryable</th>'
                 "<th>Meaning</th></tr></thead><tbody>"]
        parts += rows
        extra = sorted(c for c in set(used) if c not in registry)
        for code in extra:
            parts.append(f'<tr id="code-{esc(code)}"><td><code>{esc(code)}</code></td>'
                         f"<td>—</td><td>—</td><td>not in the registry (see build "
                         "output)</td></tr>")
            self.site.required["code"].append(f"code-{code}")
        parts.append("</tbody></table>")
        self.site.add(self.page, "Error codes", "".join(parts), self.nav(), self.commit)

    def page_index(self) -> None:
        self.page = "index.html"
        names = {"native-api": "Native API", "app-api": "App API",
                 "events": "Events", "storage": "Storage"}
        parts = ["<h1>Spotledger contracts</h1><p>Browsable documentation for the "
                 "runtime's contracts: the Native API with realtime, the event and "
                 "outbox schema, the app-facing crate and the storage backend, "
                 "with the ADRs and version history beside them.</p>"]
        for key in self.contracts:
            links = []
            if key == "native-api":
                links.append(self.link("native/index.html", "", "OpenAPI reference"))
                links.append(self.link("realtime.html", "", "Realtime protocol"))
            else:
                links.append(self.link(f"{key}.html", "", "Overview"))
                if CRATE_OF.get(key) in self.rust:
                    crate = CRATE_OF[key]
                    links.append(self.link(f"rust/{crate}/index.html", "", "rustdoc"))
            if key in self.changelogs:
                links.append(self.link(f"changelog/{key}.html", "", "changelog"))
            adr = ADR_OF.get(key)
            if adr and adr in {p.stem for p in self.adrs}:
                links.append(self.link(f"adr/{adr}.html", "", "ADR"))
            badge = (f' <span class="badge">v{esc(self.versions.get(key, "?"))}</span>'
                     if self.versions.get(key) else "")
            parts.append(f'<section class="variant"><h2><a href="'
                         + self.at("native/index.html" if key == "native-api" else f"{key}.html")
                         + f'">{names[key]}</a>{badge}</h2><p>{BLURB[key]}</p>'
                         "<p>" + " · ".join(links) + "</p></section>")
        links = [self.link("errors.html", "", "Error codes"),
                 self.link("adr/index.html", "", "Decision records")]
        parts.append('<section class="variant"><h2>Reference</h2><p>'
                     + " · ".join(links) + "</p></section>")
        self.site.add(self.page, "Spotledger contracts", "".join(parts), self.nav(), self.commit)

    def nav(self) -> list[tuple[str, list[tuple[str, str]]]]:
        """Sidebar: Overview, Native API (tags + schemas), Realtime, the other
        contracts, ADRs, Error codes."""
        groups: list[tuple[str, list[tuple[str, str]]]] = [("", [("index.html", "Overview")])]
        native = [("native/index.html", "Native API")]
        native += [(f"native/tag-{tag}.html", f"· {tag}") for tag, _ in self.ops_by_tag()]
        native += [("native/schemas/index.html", "Schemas")]
        groups.append(("Native API", native))
        groups.append(("Protocols", [("realtime.html", "Realtime")]))
        others = [(f"{key}.html", {"app-api": "App API", "events": "Events",
                                   "storage": "Storage"}[key])
                  for key in ("app-api", "events", "storage") if key in self.contracts]
        if others:
            groups.append(("Contracts", others))
        if self.adrs:
            groups.append(("Reference", [("adr/index.html", "Decision records")]))
        groups.append(("Reference", [("errors.html", "Error codes")]))
        return groups

def build_rustdoc(root: Path, out: Path) -> set[str]:
    """cargo doc for the three documented crates, copied under out/rust/; the
    acceptance command runs this (without --skip-rustdoc)."""
    cmd = ["cargo", "doc", "--no-deps", "-p", "spot-app-api",
           "-p", "spot-storage-contract", "-p", "spot-events-contract"]
    proc = subprocess.run(cmd, cwd=root)
    if proc.returncode != 0:
        sys.exit("docsite: cargo doc failed; see the cargo output above")
    src = root / "target" / "doc"
    if not (src / "help.html").exists():
        sys.exit("docsite: cargo finished but target/doc is missing; cannot link rustdoc")
    shutil.copytree(src, out / "rust")
    crates = {c for c in CRATE_OF.values() if (out / "rust" / c / "index.html").exists()}
    if not crates:
        sys.exit("docsite: cargo ran but no documented crate landed in target/doc")
    return crates


def available_contracts(root: Path) -> tuple[str, ...]:
    return tuple(key for key in KEYS if (root / "contracts" / key).is_dir())


def load_inputs(root: Path) -> tuple[dict, dict, list[str], dict[str, str], set[str], list[Path]]:
    spec_file = root / "contracts" / "native-api" / "openapi.yaml"
    spec = yaml.safe_load(spec_file.read_text(encoding="utf-8"))
    if not isinstance(spec, dict) or not isinstance(spec.get("components"), dict):
        sys.exit(f"docsite: {spec_file} does not look like an OpenAPI document")
    used = [code for _, code in used_code_nodes(spec)]
    registry = spec.get("x-spot-error-codes") or {}
    versions: dict[str, str] = {}
    changelogs: set[str] = set()
    for key in available_contracts(root):
        version_file = root / "contracts" / key / "VERSION"
        if version_file.exists():
            versions[key] = version_file.read_text().strip()
        if (root / "contracts" / key / "CHANGELOG.md").exists():
            changelogs.add(key)
    if "native-api" in available_contracts(root) and "native-api" not in versions:
        versions["native-api"] = str((spec.get("info") or {}).get("version", ""))
    adrs = sorted((root / "docs" / "adr").glob("ADR-*.md")) \
        if (root / "docs" / "adr").is_dir() else []
    return spec, registry, used, versions, changelogs, adrs

def prepare_out(out: Path) -> Path:
    """Create or wipe --out; a plain non-empty directory without the marker
    file is refused, so a typo can never cost unrelated files."""
    out = out.resolve()
    if out.exists() and any(out.iterdir()):
        if not (out / MARKER).exists():
            sys.exit(f"docsite: --out {out} exists and is not a docsite build "
                     f"(no {MARKER} file); delete it or pick another directory")
        shutil.rmtree(out)
    out.mkdir(parents=True)
    (out / MARKER).write_text("Spotledger contract documentation site "
                              "(safe to delete whole).\n")
    return out


def build_site(root: Path, out: Path, skip_rustdoc: bool = False,
               commit: str | None = None) -> tuple[Site, set[str]]:
    """Generate the whole site into `out`; return (the built site, the rustdoc
    crates that were copied). `root` may point at a fixture tree with the same
    path layout for tests."""
    out = prepare_out(out)
    spec, registry, used, versions, changelogs, adrs = load_inputs(root)
    jinja = Environment(loader=FileSystemLoader(HERE / "templates"),
                        undefined=StrictUndefined, keep_trailing_newline=True,
                        autoescape=False)
    site = Site(root, jinja)
    contracts = available_contracts(root)
    rust: set[str] = set()
    if not skip_rustdoc:
        rust = build_rustdoc(root, out)
    gen = site.gen = Gen(site, spec, root, rust, contracts, adrs, commit,
                         versions=versions, changelogs=changelogs)
    gen.page_index()
    gen.page_native()
    for tag, _ in gen.ops_by_tag():
        gen.page_tag(tag)
    gen.page_schemas()
    for name in (spec.get("components", {}).get("schemas") or {}):
        gen.page_schema(name)
    gen.page_realtime()
    for key in contracts:
        if key != "native-api":
            gen.page_contract(key)
    for key in sorted(changelogs):
        gen.page_changelog(key)
    if adrs:
        gen.page_adrs()
    for adr in adrs:
        gen.page_adr(adr)
    gen.page_errors(registry, used)
    (out / "style.css").write_text(
        (HERE / "templates" / "site.css").read_text(encoding="utf-8"),
        encoding="utf-8")
    for rel in sorted(site.pages):
        path = out / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(site.pages[rel], encoding="utf-8")
    return site, rust

def run_check(out: Path, site: Site) -> list[str]:
    """Verify what the generated HTML promises: every href/src target exists,
    every fragment lands on an id (internal pages only; rustdoc ships with
    upstream ids we do not control), and the anchors the build registered for
    ops/schemas/error codes/realtime messages are present. Returns problems in
    stable order."""
    pages: dict[str, str] = {}
    for path in sorted(out.rglob("*.html")):
        rel = path.relative_to(out).as_posix()
        if rel.startswith("rust/"):
            continue  # generated rustdoc, linked by file only
        pages[rel] = path.read_text(encoding="utf-8")
    ids = {rel: set(re.findall(r'id="([^"]+)"', html)) for rel, html in pages.items()}
    problems: list[str] = []
    for rel in sorted(pages):
        directory = rel.rsplit("/", 1)[0] if "/" in rel else ""
        for url in re.findall(r'(?:href|src)="([^"]+)"', pages[rel]):
            if url.startswith(("data:", "http:", "https:", "mailto:")):
                continue
            page_part, _, frag = url.partition("#")
            if page_part:
                target = posixpath.normpath(posixpath.join(directory, page_part))
            else:
                target = rel
            if target.startswith("rust/"):
                if not (out / target).exists():
                    problems.append(f"{rel}: rustdoc link {url!r} target is missing")
                continue
            if not (out / target).exists():
                problems.append(f"{rel}: link {url!r} has no target "
                                f"(expected file {target!r})")
                continue
            if not target.endswith(".html") or target not in pages:
                continue
            if frag and frag not in ids[target]:
                problems.append(f"{rel}: anchor {url!r} has no id on {target}")
    have = set().union(*ids.values()) if ids else set()
    for kind in ("op", "schema", "code", "msg"):
        for anchor in site.required[kind]:
            if anchor not in have:
                problems.append(f"anchor {anchor!r} ({kind}) was promised but is "
                                "not in the generated site")
    return list(dict.fromkeys(problems))


def default_commit() -> str | None:
    """Short commit hash for the footer, or None (e.g. a tarball checkout)."""
    try:
        proc = subprocess.run(["git", "rev-parse", "--short", "HEAD"],
                              cwd=REPO_ROOT, capture_output=True, text=True)
    except OSError:
        return None
    if proc.returncode == 0:
        return proc.stdout.strip() or None
    return None

def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Build the Spotledger contract documentation site (static HTML).")
    parser.add_argument("--out", required=True, type=Path,
                        help="output directory; an existing docsite build there is "
                             "replaced, other content is refused for safety")
    parser.add_argument("--check", action="store_true",
                        help="after generating, verify every internal link and anchor; "
                             "exit non-zero and print problems")
    parser.add_argument("--skip-rustdoc", action="store_true",
                        help="reuse the existing target/doc rustdoc instead of running "
                             "cargo (used by tests)")
    parser.add_argument("--commit", default=None,
                        help="override the commit hash shown in the page footers")
    parser.add_argument("--root", type=Path, default=REPO_ROOT,
                        help="repository root (defaults to this checkout; tests may "
                             "pass a fixture tree with the same layout)")
    args = parser.parse_args(argv)
    root = args.root.resolve()
    out = args.out if args.out.is_absolute() else Path.cwd() / args.out
    commit = args.commit if args.commit is not None else default_commit()
    site, rust = build_site(root, out, skip_rustdoc=args.skip_rustdoc, commit=commit)
    problems = run_check(out, site) if args.check else site.problems
    for problem in problems:
        print("docsite:", problem)
    if problems:
        print(f"docsite: {len(problems)} problem(s); build failed checks",
              file=sys.stderr)
        return 1
    suffix = f" ({len(rust)} rustdoc crate(s))" if rust else ""
    print(f"docsite: wrote {len(site.pages)} page(s) to {out}{suffix}")
    return 0


if __name__ == "__main__":
    sys.exit(main())