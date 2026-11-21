"""Canonical JSON, exactly as ``crates/framework/spot-bundle-signing/src/canonical.rs`` writes it.

The signed bytes of a component manifest are a domain prefix followed by this encoding, so the C#
host and this tool must produce the same bytes for the same fields. That is not a matter of taste:
a whitespace difference is a signature that does not verify. The rules are

* an object's keys are sorted (ordinal, which is byte order for ASCII keys) and no key repeats;
* no whitespace anywhere;
* integers only — there is no number form that holds a fraction, so money and quantities cannot be
  signed by accident;
* ``null`` where a document means "no end", as serde_json writes it: it is neither a number nor a
  string, so a window's open end cannot be read as a date of zero;
* booleans as ``true``/``false``;
* strings escaped the way serde_json escapes them: ``"``, ``\\``, ``\\b``, ``\\f``, ``\\n``, ``\\r``,
  ``\\t``, every other code point below U+0020 as ``\\u00xx`` in lower-case hex, and everything else
  (including non-ASCII) as raw UTF-8.

The tests check this encoder against the bytes the Rust crate signed (``signed_bytes_hex`` in the
checked-in vector file), which is the only check that matters.
"""

from __future__ import annotations

from typing import Any, Final

#: The domain prefix of a component package manifest's signed bytes.
MANIFEST_DOMAIN: Final = "SPOT-COMPONENT-MANIFEST-V1\n"

#: The domain prefix of a publisher certificate's signed bytes.
CERTIFICATE_DOMAIN: Final = "SPOT-PUBLISHER-CERT-V1\n"

#: The domain prefix of a revocation list's signed bytes.
REVOCATION_DOMAIN: Final = "SPOT-REVOCATION-LIST-V1\n"

_ESCAPES: Final = {
    '"': '\\"',
    "\\": "\\\\",
    "\b": "\\b",
    "\f": "\\f",
    "\n": "\\n",
    "\r": "\\r",
    "\t": "\\t",
}


def encode_string(text: str) -> str:
    """``text`` as a JSON string literal, escaped the way serde_json escapes one."""
    parts: list[str] = ['"']
    for character in text:
        escaped = _ESCAPES.get(character)
        if escaped is not None:
            parts.append(escaped)
        elif character < " ":
            parts.append("\\u%04x" % ord(character))
        else:
            parts.append(character)
    parts.append('"')
    return "".join(parts)


def encode(value: Any) -> str:
    """``value`` as canonical JSON text.

    ``bool`` is checked before ``int`` because ``True`` is an ``int`` in Python and ``true`` is not a
    number in JSON. A ``float`` is refused rather than rounded: the schema has no use for one and a
    silently rounded number is a signature over something the publisher did not sign.
    """
    if value is True:
        return "true"
    if value is False:
        return "false"
    if value is None:
        # serde_json's null, which is what a document writes where a window has no end. It is not a
        # number and not a string, so a reader cannot confuse it with a date of zero.
        return "null"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str):
        return encode_string(value)
    if isinstance(value, dict):
        keys = list(value.keys())
        if any(not isinstance(key, str) or not key for key in keys):
            raise ValueError("a canonical JSON key is a non-empty string")
        if len(set(keys)) != len(keys):
            raise ValueError("a canonical JSON object has no repeated key")
        return "{" + ",".join(
            encode_string(key) + ":" + encode(value[key]) for key in sorted(keys)
        ) + "}"
    if isinstance(value, (list, tuple)):
        return "[" + ",".join(encode(item) for item in value) + "]"
    raise TypeError(f"no canonical JSON form for {type(value).__name__}")


def signed_bytes(domain: str, value: Any) -> bytes:
    """``domain`` followed by the UTF-8 bytes of the canonical encoding of ``value``."""
    if not domain:
        raise ValueError("the signing domain is not empty")
    return domain.encode("utf-8") + encode(value).encode("utf-8")
