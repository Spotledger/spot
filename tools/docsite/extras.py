"""The pages the first site left out (WP-DX-04.T2): the error-code catalogue, the UI extension
contract, the connector WIT, the app format references, the operator guides, a flat search index
and the freshness gate for contracts.

`build.py` creates one :class:`Extras` per build, asks it for its sidebar groups, renders its
pages through the shared template and writes its ``index.json``. Everything here is read from the
repository (nothing is invented), is ordered deterministically and carries no timestamp.
"""
from __future__ import annotations

import json
import posixpath
import re
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

import wit as witreader

CATALOGUE_DIR = "crates/framework/spot-errors/catalogue"
WIT_FILE = "crates/framework/spot-connectors/wit/connector.wit"
SDK_MATRIX = "sdk/templates/MATRIX.md"
MANIFEST_FORMAT = "crates/framework/spot-manifest/FORMAT.md"
FIELD_KIND_SOURCE = "crates/framework/spot-meta/src/definition.rs"
GUIDE_INDEX = "docs/running-spot.md"
GUIDE_DIR = "docs/running-spot"
EXTRA_GUIDES = ()
KNOWN_GAPS = Path(__file__).resolve().parent / "known_gaps.toml"
# Contracts the first site renders under their own names; every other directory of contracts/ that
# carries a VERSION file gets the generic page of this module.
FIRST_SITE_KEYS = ("native-api", "app-api", "events", "storage")
UI_EXT = "ui-ext"
SECTIONS = {
    "": "Overview", "native": "Native API", "errors": "Errors", "guides": "Running Spot",
    "adr": "Decision records", "changelog": "Changelogs", "formats": "Formats",
    "wit": "Connector", "ui-ext": "UI extension", "rust": "Rust API",
}


@dataclass
class Tools:
    """The helpers of build.py, passed in so this module never imports it."""
    esc: Callable[[object], str]
    md_block: Callable[[str], str]
    md_inline: Callable[[str], str]
    json_code: Callable[[object], str]


@dataclass
class Entry:
    code: str
    cls: str
    summary: str
    explanation_key: str
    params: list[str]
    remedy: str
    since: str
    retired: bool
    file: str

    @property
    def prefix(self) -> str:
        return self.code.split(".", 1)[0]


def load_known_gaps() -> dict[str, int]:
    try:
        data = tomllib.loads(KNOWN_GAPS.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError):
        return {}
    return {k: int(v) for k, v in data.items() if isinstance(v, int)}


class Extras:
    def __init__(self, gen, tools: Tools, root: Path, contract_codes: set[str],
                 known_gaps: dict[str, int] | None = None) -> None:
        self.gen = gen
        self.site = gen.site
        self.t = tools
        self.root = root
        self.contract_codes = contract_codes
        self.known_gaps = load_known_gaps() if known_gaps is None else known_gaps
        self.entries: list[Entry] = []
        self.generic: list[tuple[str, str]] = []  # (key, version) of contracts with a generic page
        self.ui_version = ""
        self.guides: list[tuple[str, Path]] = []  # (page stem, source file)
        self.guide_pages: dict[Path, str] = {}  # source file -> page path
        self.schemas: dict[str, dict] = {}
        self.wit: witreader.Wit | None = None
        self.field_kinds: list[tuple[str, str]] = []
        self.have_formats = False

    # ------------------------------------------------------------------ loading
    def prepare(self) -> None:
        self._load_catalogue()
        self._discover_contracts()
        self._load_guides()
        self._load_ui_ext()
        self._load_wit()
        self._load_formats()

    def _load_catalogue(self) -> None:
        folder = self.root / CATALOGUE_DIR
        if not folder.is_dir():
            return
        seen: dict[str, str] = {}
        for toml in sorted(folder.glob("*.toml")):
            try:
                data = tomllib.loads(toml.read_text(encoding="utf-8"))
            except tomllib.TOMLDecodeError as error:
                self.site.fail(f"{toml.name}: not valid TOML ({error})")
                continue
            for raw in data.get("entry", []):
                code = str(raw.get("code", ""))
                if not code:
                    self.site.fail(f"{toml.name}: an entry has no code")
                    continue
                if code in seen:
                    self.site.fail(f"error code {code!r} is defined in both "
                                   f"{seen[code]} and {toml.name}")
                    continue
                seen[code] = toml.name
                self.entries.append(Entry(
                    code=code, cls=str(raw.get("class", "")), summary=str(raw.get("summary", "")),
                    explanation_key=str(raw.get("explanation_key", "")),
                    params=[str(p) for p in raw.get("params", [])],
                    remedy=str(raw.get("remedy", "")), since=str(raw.get("since", "")),
                    retired=bool(raw.get("retired", False)), file=toml.name))
        self.entries.sort(key=lambda e: e.code)

    def _discover_contracts(self) -> None:
        base = self.root / "contracts"
        if not base.is_dir():
            return
        for directory in sorted(p for p in base.iterdir() if p.is_dir()):
            key = directory.name
            version_file = directory / "VERSION"
            if not version_file.is_file() or key in FIRST_SITE_KEYS:
                continue
            version = version_file.read_text(encoding="utf-8").strip()
            if key == UI_EXT:
                self.ui_version = version
            else:
                self.generic.append((key, version))

    def _load_guides(self) -> None:
        index = self.root / GUIDE_INDEX
        if index.is_file():
            self.guides.append(("running-spot", index))
        folder = self.root / GUIDE_DIR
        if folder.is_dir():
            for path in sorted(folder.glob("*.md")):
                self.guides.append((path.stem, path))
        for relative in EXTRA_GUIDES:
            path = self.root / relative
            if path.is_file():
                self.guides.append((path.stem, path))
        stems = [stem for stem, _ in self.guides]
        for stem in sorted({s for s in stems if stems.count(s) > 1}):
            self.site.fail(f"two guides would both be rendered as guides/{stem}.html")
        self.guide_pages = {Path(posixpath.normpath(path.as_posix())): f"guides/{stem}.html"
                            for stem, path in self.guides}

    def _load_ui_ext(self) -> None:
        folder = self.root / "contracts" / UI_EXT / "schema"
        if not folder.is_dir():
            return
        for path in sorted(folder.glob("*.json")):
            try:
                self.schemas[path.stem] = json.loads(path.read_text(encoding="utf-8"))
            except json.JSONDecodeError as error:
                self.site.fail(f"contracts/{UI_EXT}/schema/{path.name}: not valid JSON ({error})")

    def _load_wit(self) -> None:
        path = self.root / WIT_FILE
        if not path.is_file():
            return
        try:
            self.wit = witreader.parse(path.read_text(encoding="utf-8"))
        except witreader.WitError as error:
            self.site.fail(f"{WIT_FILE}: {error}")

    def _load_formats(self) -> None:
        if (self.root / MANIFEST_FORMAT).is_file():
            self.have_formats = True
        source = self.root / FIELD_KIND_SOURCE
        if not source.is_file():
            return
        text = source.read_text(encoding="utf-8")
        match = re.search(r"pub enum FieldType\s*\{(.*?)\n\}", text, flags=re.S)
        if not match:
            self.site.fail(f"{FIELD_KIND_SOURCE}: cannot find `pub enum FieldType`")
            return
        doc: list[str] = []
        for line in match.group(1).splitlines():
            stripped = line.strip()
            if stripped.startswith("///"):
                doc.append(stripped[3:].strip())
            elif stripped.startswith("#[serde(rename"):
                rename = re.search(r'rename\s*=\s*"([^"]+)"', stripped)
                if rename:
                    self.field_kinds.append((rename.group(1), " ".join(doc)))
                    doc = []
            elif stripped and not stripped.startswith("#["):
                doc = []
        if not self.field_kinds:
            self.site.fail(f"{FIELD_KIND_SOURCE}: no field kind with a serde name found")

    # --------------------------------------------------------------- navigation
    def nav_groups(self) -> list[tuple[str, list[tuple[str, str]]]]:
        groups: list[tuple[str, list[tuple[str, str]]]] = []
        if self.entries:
            groups.append(("Errors", [("errors/index.html", "Error catalogue")]))
        more = [(f"{key}.html", key) for key, _ in self.generic]
        if self.ui_version:
            more.insert(0, (f"{UI_EXT}.html", "UI extension"))
        if more:
            groups.append(("More contracts", more))
        if self.wit is not None:
            groups.append(("Connector", [("wit/connector.html", "Connector WIT")]))
        formats = []
        if self.have_formats:
            formats.append(("formats/manifest.html", "App manifest"))
        if self.field_kinds:
            formats.append(("formats/doctype.html", "DocType field kinds"))
        if formats:
            groups.append(("Formats", formats))
        if self.guides:
            groups.append(("Running Spot", [("guides/index.html", "Guides")]))
        return groups

    # ------------------------------------------------------------------ helpers
    def _page(self, path: str, title: str, content: str) -> None:
        self.gen.page = path
        self.site.add(path, title, content, self.gen.nav(), self.gen.commit)

    def _crumb(self, label: str, target: str) -> str:
        return ('<p class="crumbs"><a href="' + self.gen.at("index.html") + '">Home</a> › '
                f'<a href="{self.gen.at(target)}">{self.t.esc(label)}</a></p>')

    # -------------------------------------------------------------------- render
    def render(self) -> None:
        if self.entries:
            self._errors()
        for key, version in self.generic:
            self._generic_contract(key, version)
        if self.ui_version:
            self._ui_ext()
        if self.wit is not None:
            self._wit_page()
        if self.have_formats:
            self._manifest_page()
        if self.field_kinds:
            self._doctype_page()
        if self.guides:
            self._guides()
        self._freshness()

    # ------------------------------------------------------------ error catalogue
    def _errors(self) -> None:
        esc = self.t.esc
        by_prefix: dict[str, list[Entry]] = {}
        for entry in self.entries:
            by_prefix.setdefault(entry.prefix, []).append(entry)
        known = {e.code for e in self.entries}
        missing = sorted(code for code in self.contract_codes if code not in known)
        allowed = self.known_gaps.get("contract_codes_without_catalogue_entry")
        if allowed is None:
            self.site.fail("tools/docsite/known_gaps.toml has no "
                           "contract_codes_without_catalogue_entry number")
        elif len(missing) > allowed:
            self.site.fail(f"{len(missing)} native API error codes have no catalogue entry; "
                           f"known_gaps.toml allows {allowed}: "
                           + ", ".join(missing[:10]) + (" ..." if len(missing) > 10 else ""))
        self.gen.page = "errors/index.html"
        parts = ["<h1>Error catalogue</h1>",
                 f"<p>{len(self.entries)} codes in {len(by_prefix)} groups, read from "
                 f"<code>{CATALOGUE_DIR}</code>. A code's first segment is its group.</p>",
                 '<table><thead><tr><th>Group</th><th>Codes</th></tr></thead><tbody>']
        for prefix in sorted(by_prefix):
            parts.append(f"<tr><td>{self.gen.link(f'errors/{prefix}.html', '', f'<code>{esc(prefix)}</code>')}"
                         f"</td><td>{len(by_prefix[prefix])}</td></tr>")
        parts.append("</tbody></table>")
        for prefix in sorted(by_prefix):
            parts.append(f'<h2 id="group-{esc(prefix)}">{esc(prefix)}</h2><table><thead><tr>'
                         "<th>Code</th><th>Class</th><th>Since</th><th>Summary</th></tr></thead><tbody>")
            for entry in by_prefix[prefix]:
                parts.append(
                    f"<tr><td>{self.gen.link(f'errors/{prefix}.html', f'#code-{entry.code}', f'<code>{esc(entry.code)}</code>')}"
                    f"</td><td>{esc(entry.cls)}</td><td>{esc(entry.since)}</td>"
                    f"<td>{self.t.md_inline(entry.summary)}</td></tr>")
            parts.append("</tbody></table>")
        parts.append('<h2 id="contract-codes-without-an-entry">Contract codes without a catalogue entry</h2>'
                     "<p>Codes the native API contract names that no catalogue file defines "
                     f"({len(missing)}; the known-gaps number is {allowed}).</p>")
        parts.append("<ul>" + "".join(f"<li><code>{esc(c)}</code></li>" for c in missing) + "</ul>"
                     if missing else "<p>None.</p>")
        self.site.add("errors/index.html", "Error catalogue", "".join(parts),
                      self.gen.nav(), self.gen.commit)
        for prefix, entries in sorted(by_prefix.items()):
            self.gen.page = f"errors/{prefix}.html"
            body = [self._crumb("Error catalogue", "errors/index.html"),
                    f"<h1>{esc(prefix)}</h1>"]
            for entry in entries:
                self.site.required["code"].append(f"code-{entry.code}")
                params = ", ".join(f"<code>{esc(p)}</code>" for p in entry.params) or "none"
                retired = ' <span class="badge">retired</span>' if entry.retired else ""
                body.append(
                    f'<section class="variant" id="code-{esc(entry.code)}"><h2><code>{esc(entry.code)}</code>'
                    f"{retired}</h2><p>{self.t.md_inline(entry.summary)}</p><ul>"
                    f"<li>class: <code>{esc(entry.cls)}</code></li><li>since: {esc(entry.since)}</li>"
                    f"<li>parameters: {params}</li>"
                    f"<li>remedy: {self.t.md_inline(entry.remedy)}</li>"
                    f"<li>explanation key: <code>{esc(entry.explanation_key)}</code></li>"
                    f"<li>owner: <code>{CATALOGUE_DIR}/{esc(entry.file)}</code></li></ul></section>")
            self.site.add(self.gen.page, f"Errors · {prefix}", "".join(body),
                          self.gen.nav(), self.gen.commit)

    # ---------------------------------------------------------- generic contracts
    def _generic_contract(self, key: str, version: str) -> None:
        esc = self.t.esc
        directory = self.root / "contracts" / key
        self.gen.page = f"{key}.html"
        parts = ['<p class="crumbs"><a href="' + self.gen.at("index.html") + '">Home</a></p>',
                 f"<h1>{esc(key)}</h1>", f'<p><span class="badge">v{esc(version)}</span></p>',
                 f"<p>Contract directory: <code>contracts/{esc(key)}</code></p>"]
        readme = directory / "README.md"
        if readme.is_file():
            parts.append(self.t.md_block(readme.read_text(encoding="utf-8", errors="replace")))
        spec_file = directory / "openapi.yaml"
        if spec_file.is_file():
            try:
                import yaml
                spec = yaml.safe_load(spec_file.read_text(encoding="utf-8"))
            except Exception as error:  # noqa: BLE001 - reported as a build problem
                self.site.fail(f"contracts/{key}/openapi.yaml: cannot be read ({error})")
                spec = {}
            info = (spec or {}).get("info") or {}
            if info.get("description"):
                parts.append(self.t.md_block(str(info["description"])))
            rows = []
            for route, item in sorted(((spec or {}).get("paths") or {}).items()):
                for method in ("get", "put", "post", "delete", "patch", "head", "options"):
                    op = (item or {}).get(method)
                    if isinstance(op, dict):
                        rows.append(f"<tr><td><code>{method.upper()}</code></td><td><code>{esc(route)}</code></td>"
                                    f"<td><code>{esc(op.get('operationId', ''))}</code></td>"
                                    f"<td>{self.t.md_inline(str(op.get('summary', '')))}</td></tr>")
            if rows:
                parts.append("<h2>Operations</h2><table><thead><tr><th>Method</th><th>Path</th>"
                             "<th>Operation</th><th>Summary</th></tr></thead><tbody>"
                             + "".join(rows) + "</tbody></table>")
        if (directory / "CHANGELOG.md").is_file():
            parts.append(f'<h2>Changelog</h2>'
                         f'<p>{self.gen.link(f"changelog/{key}.html", "", "CHANGELOG.md")}</p>')
        self.site.add(self.gen.page, key, "".join(parts), self.gen.nav(), self.gen.commit)
        if (directory / "CHANGELOG.md").is_file():
            self._changelog(key, version)

    def _changelog(self, key: str, version: str) -> None:
        self.gen.page = f"changelog/{key}.html"
        text = (self.root / "contracts" / key / "CHANGELOG.md").read_text(encoding="utf-8")
        target = f"{UI_EXT}.html" if key == UI_EXT else f"{key}.html"
        body = [self._crumb(key, target), f"<h1>{self.t.esc(key)} — contract changelog</h1>",
                f'<p><span class="badge">v{self.t.esc(version)}</span></p>', self.t.md_block(text)]
        self.site.add(self.gen.page, f"Changelog · {key}", "".join(body),
                      self.gen.nav(), self.gen.commit)

    # ------------------------------------------------------------- UI extension
    def _schema_target(self, ref: str, current: str, where: str) -> str | None:
        """The href of a `$ref` from the schema file `current`, or None (and a problem)."""
        file_part, _, pointer = ref.partition("#")
        name = Path(file_part).stem if file_part else current
        schema = self.schemas.get(name)
        if schema is None:
            self.site.fail(f"{where}: $ref {ref!r} names a schema file that does not exist")
            return None
        anchor = ""
        if pointer:
            parts = pointer.strip("/").split("/")
            node: object = schema
            for part in parts:
                if not isinstance(node, dict) or part not in node:
                    self.site.fail(f"{where}: $ref {ref!r} does not resolve")
                    return None
                node = node[part]
            if len(parts) == 2 and parts[0] == "$defs":
                anchor = f"#def-{parts[1]}"
        return self.gen.at(f"{UI_EXT}/schema-{name}.html") + anchor

    def _type_cell(self, node: object, current: str, where: str) -> str:
        esc = self.t.esc
        if not isinstance(node, dict):
            return esc(node)
        if "$ref" in node:
            href = self._schema_target(str(node["$ref"]), current, where)
            label = esc(str(node["$ref"]).split("/")[-1])
            return f'<a href="{href}"><code>{label}</code></a>' if href else f"<code>{label}</code>"
        bits: list[str] = []
        if "const" in node:
            bits.append(f"const <code>{esc(json.dumps(node['const']))}</code>")
        if "enum" in node:
            bits.append("one of " + ", ".join(f"<code>{esc(json.dumps(v))}</code>" for v in node["enum"]))
        if "type" in node:
            kind = node["type"] if isinstance(node["type"], str) else " | ".join(node["type"])
            bits.append(f"<code>{esc(kind)}</code>")
        if isinstance(node.get("items"), dict):
            bits.append("of " + self._type_cell(node["items"], current, where))
        for key in ("oneOf", "anyOf", "allOf"):
            if isinstance(node.get(key), list):
                bits.append(f"{key}: " + "; ".join(self._type_cell(n, current, where) for n in node[key]))
        for key in ("format", "pattern", "minLength", "maxLength", "minimum", "maximum",
                    "minItems", "maxItems", "maxProperties", "minProperties"):
            if key in node:
                bits.append(f"{key} <code>{esc(node[key])}</code>")
        if node.get("additionalProperties") is False:
            bits.append("closed")
        return "; ".join(bits) or "—"

    def _props_table(self, node: dict, current: str, where: str) -> str:
        props = node.get("properties")
        if not isinstance(props, dict) or not props:
            return ""
        required = set(node.get("required") or [])
        rows = []
        for name in sorted(props):
            prop = props[name]
            desc = self.t.md_inline(str(prop.get("description", ""))) if isinstance(prop, dict) else ""
            rows.append(f"<tr><td><code>{self.t.esc(name)}</code></td>"
                        f"<td>{self._type_cell(prop, current, f'{where}/properties/{name}')}</td>"
                        f"<td>{'yes' if name in required else ''}</td><td>{desc}</td></tr>")
        return ("<table><thead><tr><th>Property</th><th>Type</th><th>Required</th><th>Meaning</th>"
                "</tr></thead><tbody>" + "".join(rows) + "</tbody></table>")

    def _ui_ext(self) -> None:
        esc = self.t.esc
        directory = self.root / "contracts" / UI_EXT
        self.gen.page = f"{UI_EXT}.html"
        parts = ['<p class="crumbs"><a href="' + self.gen.at("index.html") + '">Home</a></p>',
                 "<h1>UI extension contract</h1>",
                 f'<p><span class="badge">v{esc(self.ui_version)}</span></p>',
                 f"<p>Contract directory: <code>contracts/{UI_EXT}</code></p>"]
        readme = directory / "README.md"
        if readme.is_file():
            parts.append(self.t.md_block(readme.read_text(encoding="utf-8", errors="replace")))
        parts.append("<h2>Pages</h2><ul>")
        for name in sorted(self.schemas):
            parts.append("<li>" + self.gen.link(f"{UI_EXT}/schema-{name}.html", "", f"schema <code>{esc(name)}</code>") + "</li>")
        parts.append("<li>" + self.gen.link(f"{UI_EXT}/examples.html", "", "examples") + "</li>")
        if (directory / "errors.md").is_file():
            parts.append("<li>" + self.gen.link(f"{UI_EXT}/errors.html", "", "errors (<code>uiext.*</code>)") + "</li>")
        if (directory / "CHANGELOG.md").is_file():
            parts.append("<li>" + self.gen.link(f"changelog/{UI_EXT}.html", "", "changelog") + "</li>")
        parts.append("</ul>")
        self.site.add(self.gen.page, "UI extension contract", "".join(parts),
                      self.gen.nav(), self.gen.commit)
        for name, schema in sorted(self.schemas.items()):
            self.gen.page = f"{UI_EXT}/schema-{name}.html"
            where = f"contracts/{UI_EXT}/schema/{name}.json"
            body = [self._crumb("UI extension contract", f"{UI_EXT}.html"),
                    f"<h1>{esc(schema.get('title', name))}</h1>",
                    f"<p><code>{esc(schema.get('$id', ''))}</code></p>",
                    self.t.md_block(str(schema.get("description", "")))]
            body.append(self._props_table(schema, name, where))
            if schema.get("additionalProperties") is False:
                body.append("<p>Closed object: no property other than those listed is allowed.</p>")
            for def_name, definition in sorted((schema.get("$defs") or {}).items()):
                body.append(f'<section class="variant" id="def-{esc(def_name)}"><h2><code>{esc(def_name)}</code></h2>')
                if isinstance(definition, dict):
                    body.append(f"<p>{self._type_cell(definition, name, f'{where}/$defs/{def_name}')}</p>")
                    if definition.get("description"):
                        body.append(self.t.md_block(str(definition["description"])))
                    body.append(self._props_table(definition, name, f"{where}/$defs/{def_name}"))
                body.append("</section>")
            self.site.add(self.gen.page, f"UI extension · {name}", "".join(body),
                          self.gen.nav(), self.gen.commit)
        self._ui_examples(directory)
        if (directory / "errors.md").is_file():
            self.gen.page = f"{UI_EXT}/errors.html"
            text = (directory / "errors.md").read_text(encoding="utf-8", errors="replace")
            self.site.add(self.gen.page, "UI extension · errors",
                          self._crumb("UI extension contract", f"{UI_EXT}.html") + self.t.md_block(text),
                          self.gen.nav(), self.gen.commit)
        if (directory / "CHANGELOG.md").is_file():
            self._changelog(UI_EXT, self.ui_version)

    def _ui_examples(self, directory: Path) -> None:
        esc = self.t.esc
        self.gen.page = f"{UI_EXT}/examples.html"
        body = [self._crumb("UI extension contract", f"{UI_EXT}.html"), "<h1>Examples</h1>",
                "<p>The shared corpus both implementations must agree on. Each file is checked "
                f"here to be well-formed JSON; <code>contracts/{UI_EXT}/check.py</code> (its own CI "
                "job) validates them against the schemas and the semantic rules.</p>"]
        base = directory / "examples"
        for group in sorted(p for p in base.iterdir() if p.is_dir()) if base.is_dir() else []:
            body.append(f"<h2>{esc(group.name)}</h2>")
            for path in sorted(group.glob("*.json")):
                try:
                    data = json.loads(path.read_text(encoding="utf-8"))
                except json.JSONDecodeError as error:
                    self.site.fail(f"contracts/{UI_EXT}/examples/{group.name}/{path.name}: "
                                   f"not valid JSON ({error})")
                    continue
                comment = data.get("$comment", "") if isinstance(data, dict) else ""
                body.append(f'<section class="variant" id="ex-{esc(group.name)}-{esc(path.stem)}">'
                            f"<h3><code>{esc(group.name)}/{esc(path.name)}</code></h3>"
                            + (f"<p>{esc(comment)}</p>" if comment else "")
                            + f"<pre><code>{self.t.json_code(data)}</code></pre></section>")
        self.site.add(self.gen.page, "UI extension · examples", "".join(body),
                      self.gen.nav(), self.gen.commit)

    # ---------------------------------------------------------------- connector
    def _wit_page(self) -> None:
        esc = self.t.esc
        assert self.wit is not None
        self.gen.page = "wit/connector.html"
        text = (self.root / WIT_FILE).read_text(encoding="utf-8")
        body = ['<p class="crumbs"><a href="' + self.gen.at("index.html") + '">Home</a></p>',
                "<h1>Connector WIT</h1>", f'<p><span class="badge">{esc(self.wit.package)}</span></p>',
                f"<p>Source: <code>{WIT_FILE}</code>. The outline below is read from the file; the "
                "file is shown verbatim after it.</p>"]
        for scope in self.wit.scopes:
            body.append(f'<h2 id="{esc(scope.kind)}-{esc(scope.name)}">{esc(scope.kind)} '
                        f"<code>{esc(scope.name)}</code></h2>")
            if scope.doc:
                body.append(f"<p>{self.t.md_inline(scope.doc)}</p>")
            for kind in ("enum", "record", "variant", "flags", "type", "use"):
                items = [i for i in scope.items if i.kind == kind]
                for item in items:
                    body.append(f'<section class="variant" id="{esc(kind)}-{esc(item.name)}">'
                                f"<h3>{esc(kind)} <code>{esc(item.name)}</code></h3>"
                                + (f"<p>{self.t.md_inline(item.doc)}</p>" if item.doc else "")
                                + (f"<p><code>{esc(item.detail)}</code></p>" if item.detail else ""))
                    if item.members:
                        body.append("<table><thead><tr><th>Name</th><th>Type</th><th>Meaning</th></tr></thead><tbody>"
                                    + "".join(f"<tr><td><code>{esc(n)}</code></td><td><code>{esc(t)}</code></td>"
                                              f"<td>{self.t.md_inline(d)}</td></tr>" for n, t, d in item.members)
                                    + "</tbody></table>")
                    body.append("</section>")
            funcs = [i for i in scope.items if i.kind == "func"]
            if funcs:
                body.append("<h3>Functions</h3><table><thead><tr><th>Direction</th><th>Name</th>"
                            "<th>Signature</th><th>Meaning</th></tr></thead><tbody>"
                            + "".join(f'<tr id="func-{esc(f.name)}"><td>{esc(f.direction or "—")}</td>'
                                      f"<td><code>{esc(f.name)}</code></td><td><code>{esc(f.detail)}</code></td>"
                                      f"<td>{self.t.md_inline(f.doc)}</td></tr>" for f in funcs)
                            + "</tbody></table>")
        matrix = self.root / SDK_MATRIX
        if matrix.is_file():
            body.append('<h2 id="sdk-templates">SDK templates</h2>'
                        + self.t.md_block(matrix.read_text(encoding="utf-8", errors="replace")))
        body.append(f"<h2>The file</h2><pre><code>{esc(text)}</code></pre>")
        self.site.add(self.gen.page, "Connector WIT", "".join(body), self.gen.nav(), self.gen.commit)

    # ------------------------------------------------------------------ formats
    def _manifest_page(self) -> None:
        self.gen.page = "formats/manifest.html"
        text = (self.root / MANIFEST_FORMAT).read_text(encoding="utf-8", errors="replace")
        self.site.add(self.gen.page, "App manifest format",
                      f"<p>Source: <code>{MANIFEST_FORMAT}</code>.</p>" + self.t.md_block(text),
                      self.gen.nav(), self.gen.commit)

    def _doctype_page(self) -> None:
        esc = self.t.esc
        self.gen.page = "formats/doctype.html"
        rows = "".join(f'<tr id="kind-{esc(name)}"><td><code>{esc(name)}</code></td>'
                       f"<td>{self.t.md_inline(doc)}</td></tr>" for name, doc in self.field_kinds)
        body = ("<h1>DocType field kinds</h1><p>The values of <code>type</code> in a DocType "
                f"TOML <code>[[fields]]</code> entry, read from <code>{FIELD_KIND_SOURCE}</code> "
                "(the <code>FieldType</code> enum and its doc comments).</p>"
                "<table><thead><tr><th>type</th><th>Meaning</th></tr></thead><tbody>"
                + rows + "</tbody></table>")
        self.site.add(self.gen.page, "DocType field kinds", body, self.gen.nav(), self.gen.commit)

    # ------------------------------------------------------------------- guides
    def _rewrite_links(self, source: Path, text: str) -> str:
        """Point `.md` links at the rendered guides; refuse a link to a repository file that the
        site does not render (the sentence is kept, the link is dropped from the output)."""
        def fix(match: re.Match[str]) -> str:
            label, target = match.group(1), match.group(2)
            if re.match(r"^[a-z][a-z0-9+.-]*:", target) or target.startswith("#"):
                return match.group(0)
            path_part, _, fragment = target.partition("#")
            resolved = Path(posixpath.normpath(posixpath.join(source.parent.as_posix(), path_part)))
            if resolved in self.guide_pages:
                href = "/" + self.guide_pages[resolved]  # made relative below
                return f"[{label}]({href}{('#' + fragment) if fragment else ''})"
            if not resolved.exists():
                self.site.fail(f"{source.relative_to(self.root).as_posix()}: link {target!r} "
                               "points at a file that does not exist")
            elif resolved.suffix == ".md":
                self.site.fail(f"{source.relative_to(self.root).as_posix()}: link {target!r} "
                               "points at a page the site does not render")
            return f"{label} (`{path_part}`)"
        return re.sub(r"\[([^\]]+)\]\(([^)\s]+)\)", fix, text)

    def _guides(self) -> None:
        esc = self.t.esc
        for stem, path in self.guides:
            page = f"guides/{stem}.html"
            self.gen.page = page
            text = self._rewrite_links(path, path.read_text(encoding="utf-8", errors="replace"))
            html = self.t.md_block(text)
            prefix = "../" * page.count("/")
            html = html.replace('href="/guides/', f'href="{prefix}guides/')
            title = re.search(r"^#\s+(.+)$", text, re.M)
            self.site.add(page, f"Guide · {title.group(1) if title else stem}",
                          self._crumb("Guides", "guides/index.html")
                          + f"<p>Source: <code>{esc(path.relative_to(self.root).as_posix())}</code>.</p>" + html,
                          self.gen.nav(), self.gen.commit)
        self.gen.page = "guides/index.html"
        items = []
        for stem, path in self.guides:
            first = path.read_text(encoding="utf-8", errors="replace")
            title = re.search(r"^#\s+(.+)$", first, re.M)
            items.append("<li>" + self.gen.link(f"guides/{stem}.html", "",
                                                esc(title.group(1) if title else stem)) + "</li>")
        self.site.add("guides/index.html", "Running Spot guides",
                      "<h1>Running Spot</h1><p>The operator guides, rendered from "
                      f"<code>{GUIDE_INDEX}</code> and <code>{GUIDE_DIR}/</code>.</p><ul>"
                      + "".join(items) + "</ul>", self.gen.nav(), self.gen.commit)

    # ---------------------------------------------------------------- freshness
    def _freshness(self) -> None:
        """Every contract directory with a VERSION file has a page showing that version."""
        base = self.root / "contracts"
        if not base.is_dir():
            return
        for directory in sorted(p for p in base.iterdir() if p.is_dir()):
            version_file = directory / "VERSION"
            if not version_file.is_file():
                continue
            key = directory.name
            version = version_file.read_text(encoding="utf-8").strip()
            page = "native/index.html" if key == "native-api" else (
                f"{UI_EXT}.html" if key == UI_EXT else f"{key}.html")
            html = self.site.pages.get(page)
            if html is None:
                self.site.fail(f"contract {key!r} has a VERSION file but no page ({page}); "
                               "see tools/docsite/README.md 'Adding a contract'")
            elif f"v{version}" not in html:
                self.site.fail(f"contract {key!r}: its page does not show version v{version}")

    # ---------------------------------------------------------------- index.json
    def index_json(self) -> str:
        entries = []
        for path, _ in sorted(self.site.pages.items()):
            if path.startswith("rust/"):
                continue
            head = path.split("/", 1)[0] if "/" in path else ""
            title = self.site.titles.get(path, path)
            entries.append({"title": title, "path": path,
                            "section": SECTIONS.get(head, "Contracts")})
        entries.sort(key=lambda e: (e["section"], e["path"]))
        return json.dumps(entries, indent=2, ensure_ascii=False) + "\n"
