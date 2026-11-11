#!/usr/bin/env python3
"""Refuse a comma-split plain scalar inside a YAML flow mapping (WP-CON-03.T12).

openapi.yaml uses compact flow mappings such as `{type: integer, description: Total digits, for decimals.}`.
In flow context a plain scalar ends at a comma, so that line parses as `description: "Total digits"` plus a
stray key `for decimals.` whose value is null. Nothing fails; the description is silently truncated.

Method (one, chosen on purpose): compose the document with PyYAML (the library pinned for check.py) and
walk the node graph. A flow mapping pair whose value is an empty null scalar and whose key text contains a
space or ends with `.` is the signature of a split sentence. Working on parsed nodes, not on raw text, means
quoted strings, block scalars, `enum: [a, b]` and commas inside brackets or quotes cannot be mistaken for it,
and the reported line is the stray key's own line.

A violation prints `file:line: message` and the exit status is 1. KNOWN_BAD (five on contract 0.9.0; the brief expected two) lists the offences present
in the contract today, keyed by their exact stray-key text (not line numbers); they are tolerated until the
contract fix lands.
"""
import sys
from pathlib import Path

import yaml

DEFAULT_PATH = Path(__file__).resolve().parents[3] / "contracts" / "native-api" / "openapi.yaml"

# remove when the contract fix lands (WP-CON-03.T11)
KNOWN_BAD = {
    "for decimals.",
    "in ETag form. It changes when rules or grants change; a client refreshes the summary when it does.",
    "or `application/octet-stream` when the upload did not name one.",
    "when one was set.",
    "documented per code.",
}


def _is_null(node):
    return isinstance(node, yaml.ScalarNode) and node.tag == "tag:yaml.org,2002:null" and node.value == ""


def find_violations(text):
    """Return [(line, key_text)] for every comma-split plain scalar in a flow mapping."""
    found = []
    seen = set()

    def walk(node, in_flow):
        if id(node) in seen:
            return
        seen.add(id(node))
        if isinstance(node, yaml.MappingNode):
            flow = in_flow or bool(node.flow_style)
            for key, value in node.value:
                if (
                    flow
                    and isinstance(key, yaml.ScalarNode)
                    and key.style is None
                    and _is_null(value)
                    and (" " in key.value or key.value.endswith("."))
                ):
                    found.append((key.start_mark.line + 1, key.value))
                walk(key, flow)
                walk(value, flow)
        elif isinstance(node, yaml.SequenceNode):
            flow = in_flow or bool(node.flow_style)
            for item in node.value:
                walk(item, flow)

    for doc in yaml.compose_all(text, Loader=yaml.SafeLoader):
        if doc is not None:
            walk(doc, False)
    return sorted(found)


def check(path, allow=KNOWN_BAD):
    """Return violation messages for the file, plus stale allow-list messages."""
    text = Path(path).read_text(encoding="utf-8")
    messages = []
    hit = set()
    for line, key in find_violations(text):
        if key in allow:
            hit.add(key)
            continue
        messages.append(
            f"{path}:{line}: flow mapping has a plain scalar split at a comma (stray null key {key!r}); "
            "quote the whole value"
        )
    for key in sorted(allow - hit):
        messages.append(f"{path}:0: the allow-list entry {key!r} is no longer needed, delete it")
    return messages


def main(argv):
    path = Path(argv[1]) if len(argv) > 1 else DEFAULT_PATH
    messages = check(path)
    for m in messages:
        print(m)
    return 1 if messages else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
