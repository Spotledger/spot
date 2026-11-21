"""The one place where contract-provided text becomes TypeScript source text.

Every string from the contract that lands in generated source (property names,
enum values, operation ids, descriptions) goes through a function in this
module, so there is exactly one place to audit (WP-WEB-10.T6, item 3).
"""

from __future__ import annotations

import json
import re

IDENTIFIER = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")

# Names the generated code itself uses at module level; a contract schema with
# one of these names would shadow or collide with it, so it is refused.
RESERVED_TYPE_NAMES = frozenset(
    {
        "Uint8Array",
        "Promise",
        "Array",
        "ReadonlyArray",
        "Object",
        "String",
        "Number",
        "Boolean",
        "Operations",
        "OperationId",
        "OperationArgs",
        "OperationResult",
        "operations",
        "Transport",
        "NativeApi",
        "HttpRequest",
        "HttpResponse",
        "ResponseHeaders",
        "FormPart",
        "FormParts",
        "CONTRACT_VERSION",
        "CONTRACT_SHA256",
        "T",
    }
)

# Names that cannot be client methods because the class itself has them.
RESERVED_METHOD_NAMES = frozenset({"constructor", "call", "transport", "baseUrl", "prototype", "__proto__"})


def ts_string(text: str) -> str:
    """A double-quoted TypeScript string literal for ``text``.

    ``json.dumps`` with ``ensure_ascii`` escapes quotes, backslashes, control
    characters and every non-ASCII code point (including U+2028/U+2029), which
    is a valid TypeScript string literal in every position.
    """
    return json.dumps(text, ensure_ascii=True)


def is_identifier(text: str) -> bool:
    return bool(IDENTIFIER.match(text))


def ts_key(text: str) -> str:
    """A property key: bare when it is a plain identifier, quoted otherwise.

    ``__proto__`` is always quoted: unquoted it is still a plain member in a
    type, but it must never look like a prototype setter in any output.
    """
    if is_identifier(text) and text != "__proto__":
        return text
    return ts_string(text)


def literal(value: object) -> str | None:
    """A TypeScript literal type for a JSON scalar, or None when not scalar."""
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return json.dumps(value)
    if isinstance(value, str):
        return ts_string(value)
    return None


def _clean(text: str) -> str:
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    text = text.replace(" ", " ").replace(" ", " ")
    # Drop control characters other than newline and tab.
    text = "".join(ch for ch in text if ch in "\n\t" or ord(ch) >= 0x20)
    # A comment ends at the first "*/": make that impossible.
    return text.replace("*/", "*\\/")


def first_paragraph(text: object, limit: int = 400) -> str:
    """The first paragraph of a description, cleaned and bounded."""
    if not isinstance(text, str):
        return ""
    paragraph = text.strip().split("\n\n", 1)[0]
    lines = [line.rstrip() for line in _clean(paragraph).split("\n")]
    out = "\n".join(lines).strip()
    if len(out) > limit:
        out = out[: limit - 3].rstrip() + "..."
    return out


def doc_comment(text: object, indent: str = "", limit: int = 400) -> list[str]:
    """Lines of a ``/** ... */`` doc comment, or an empty list for no text."""
    body = first_paragraph(text, limit)
    if not body:
        return []
    lines = body.split("\n")
    if len(lines) == 1:
        return [f"{indent}/** {lines[0]} */"]
    out = [f"{indent}/**"]
    out += [f"{indent} * {line}".rstrip() for line in lines]
    out.append(f"{indent} */")
    return out


def inline_comment(text: str, limit: int = 160) -> str:
    """A single-line ``/* ... */`` comment that cannot end early."""
    flat = " ".join(_clean(text).split())
    if len(flat) > limit:
        flat = flat[: limit - 3] + "..."
    return f"/* {flat} */"
