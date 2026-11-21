#!/usr/bin/env python3
"""Synthetic-corpus source-hygiene index and scanner (standard library only)."""

from __future__ import annotations

import argparse
import array
import bisect
import fnmatch
import gzip
import hashlib
import io
import json
import keyword
import re
import struct
import subprocess
import sys
import tokenize
import tomllib
from collections import deque
from pathlib import Path
from typing import Iterable, NamedTuple

FORMAT = 2
INDEX_MAGIC = b"SPOTHYGN"
DEFAULT_K = 25
DEFAULT_W = 20
LONG_MATCH = 40
MASK = (1 << 64) - 1
BASE = 0x9E3779B97F4A7C15  # odd, so the rolling hash is invertible modulo 2**64
REGION_GAP_FACTOR = 2  # contiguous-region chains may gap by at most 2 * w tokens
# Low-information fingerprints (WP-TST-13.T3): a winnowed window is dropped
# when it is too generic to identify a real copy on its own. Both patterns
# were measured against the false positives found with an empty allow.toml:
# 1. literal-only windows (a tuple of invented two- and three-letter syllables
#    in datagen/names.py, and long string/template runs) and
# 2. period-dominated windows, i.e. runs of one short statement shape such as
#    the datagen test assertions (period 10 covering ~100% of each window).
LOW_INFO_MAX_PERIOD = 16        # a window may repeat a period of at most this many tokens...
LOW_INFO_PERIOD_FRACTION = 0.90  # ... when >= 90% of its aligned positions repeat that period
LOW_INFO_PUNCTUATION = frozenset("!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~")
# Normalised token values that carry no identifier or keyword information:
# every literal collapses to LIT, and template/interpolation and JSON
# structure markers are pure scaffolding.
LOW_INFO_PLACEHOLDERS = frozenset(
    {
        "LIT",
        "TEMPLATE_START",
        "TEMPLATE_END",
        "TEMPLATE_EXPR_START",
        "TEMPLATE_EXPR_END",
        "INTERPOLATED_START",
        "INTERPOLATED_END",
        "INTERPOLATION_START",
        "INTERPOLATION_END",
        "JSON_OBJECT",
        "END_OBJECT",
        "JSON_ARRAY",
        "JSON_ITEM",
        "END_ARRAY",
    }
)
SUPPORTED = {
    ".py": "python",
    ".js": "javascript",
    ".jsx": "javascript",
    ".mjs": "javascript",
    ".ts": "typescript",
    ".tsx": "typescript",
    ".rs": "rust",
    ".cs": "csharp",
    ".json": "json",
}

KEYWORDS = {
    "python": set(keyword.kwlist) | set(getattr(keyword, "softkwlist", ())) | {"True", "False", "None"},
    "javascript": set("""await break case catch class const continue debugger default delete do else export extends false finally for function if import in instanceof let new null return super switch this throw true try typeof var void while with yield async of as from get set static""".split()),
    "typescript": set("""abstract any as asserts async await boolean break case catch class const constructor continue debugger declare default delete do else enum export extends false finally for from function get if implements import infer instanceof interface is keyof let module namespace never new null number object override package private protected public readonly require global return satisfies set static string super switch symbol this throw true try type typeof undefined unique unknown using var void while with yield""".split()),
    "rust": set("""as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return Self self static struct super trait true try type unsafe use where while abstract become box do final macro override priv typeof unsized virtual yield""".split()),
    "csharp": set("""abstract as base bool break byte case catch char checked class const continue decimal default delegate do double else enum event explicit extern false finally fixed float for foreach goto if implicit in int interface internal is lock long namespace new null object operator out override params private protected public readonly ref return sbyte sealed short sizeof stackalloc static string struct switch this throw true try typeof uint ulong unchecked unsafe ushort using virtual void volatile while async await var dynamic get set partial record required init where yield let from select group orderby join on equals by into global file scoped when""".split()),
}

OPERATORS = sorted(
    "=== !== >>>= **= &&= ||= ??= <<= >>= >>> => == != <= >= ++ -- && || ?? ?. :: -> += -= *= /= %= &= |= ^= << >> ** ... = + - * / % & | ^ ! ~ < > ? : . , ; ( ) [ ] { } @ #".split(),
    key=len,
    reverse=True,
)
NUMBER_RE = re.compile(
    r"(?:0[xX][0-9A-Fa-f](?:_?[0-9A-Fa-f])*|0[bB][01](?:_?[01])*|0[oO][0-7](?:_?[0-7])*|"
    r"(?:[0-9](?:_?[0-9])*(?:\.(?:[0-9](?:_?[0-9])*)?)?|\.[0-9](?:_?[0-9])*)"
    r"(?:[eE][+-]?[0-9](?:_?[0-9])*)?)(?:[a-zA-Z_][a-zA-Z0-9_]*)?"
)


class Token(NamedTuple):
    """One normalised token with its source line; a tuple for hot-loop speed."""

    value: str
    line: int


def _python_tokens(source: str) -> list[Token]:
    out: list[Token] = []
    ignored = {
        tokenize.COMMENT,
        tokenize.NL,
        tokenize.NEWLINE,
        tokenize.INDENT,
        tokenize.DEDENT,
        tokenize.ENDMARKER,
        tokenize.ENCODING,
    }
    fstring_types = {
        getattr(tokenize, name)
        for name in ("FSTRING_START", "FSTRING_MIDDLE", "FSTRING_END")
        if hasattr(tokenize, name)
    }
    stream = tokenize.generate_tokens(io.StringIO(source).readline)
    try:
        for item in stream:
            if item.type in ignored:
                continue
            if item.type == tokenize.NAME:
                value = item.string if item.string in KEYWORDS["python"] else "ID"
            elif item.type in (tokenize.NUMBER, tokenize.STRING) or item.type in fstring_types:
                value = "LIT"
            elif item.type == tokenize.OP:
                value = item.string
            else:
                continue
            out.append(Token(value, item.start[0]))
    except (tokenize.TokenError, IndentationError, SyntaxError):
        # Keep tokens emitted before a syntax error; source matching must not require compilability.
        pass
    return out


def _skip_quoted(source: str, start: int, quote: str) -> tuple[int, int]:
    """Return (end offset, newline count) for a single, double, or template quote."""
    i = start + 1
    newlines = 0
    while i < len(source):
        char = source[i]
        if char == "\\":
            if i + 1 < len(source) and source[i + 1] == "\n":
                newlines += 1
            i += 2
            continue
        if char == quote:
            return i + 1, newlines
        if char == "\n":
            newlines += 1
        i += 1
    return len(source), newlines


def _template_expression_end(source: str, start: int) -> int:
    """Find the matching } for a JavaScript `${...}` expression."""
    depth = 1
    i = start + 2
    while i < len(source):
        if source.startswith("//", i):
            end = source.find("\n", i + 2)
            i = len(source) if end < 0 else end
            continue
        if source.startswith("/*", i):
            end = source.find("*/", i + 2)
            i = len(source) if end < 0 else end + 2
            continue
        char = source[i]
        if char in "\"'`":
            i, _ = _skip_quoted(source, i, char)
            continue
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return -1


def _template_tokens(source: str, start: int, line: int, language: str) -> tuple[list[Token], int, int]:
    out = [Token("TEMPLATE_START", line)]
    i = start + 1
    chunk_line = line
    while i < len(source):
        char = source[i]
        if char == "\\":
            if i + 1 < len(source) and source[i + 1] == "\n":
                line += 1
            i += 2
            continue
        if char == "`":
            out.extend((Token("LIT", chunk_line), Token("TEMPLATE_END", line)))
            return out, i + 1, line
        if source.startswith("${", i):
            close = _template_expression_end(source, i)
            if close < 0:
                out.extend((Token("LIT", chunk_line), Token("TEMPLATE_END", line)))
                return out, len(source), line + source.count("\n", i)
            out.append(Token("LIT", chunk_line))
            out.append(Token("TEMPLATE_EXPR_START", line))
            expression = source[i + 2 : close]
            expression_line = line + source.count("\n", i + 2, close)
            out.extend(Token(token.value, expression_line + token.line - 1) for token in _generic_tokens(expression, language))
            expression_end_line = line + source.count("\n", i, close + 1)
            out.append(Token("TEMPLATE_EXPR_END", expression_end_line))
            line += source.count("\n", i, close + 1)
            i = close + 1
            chunk_line = line
            continue
        if char == "\n":
            line += 1
        i += 1
    out.extend((Token("LIT", chunk_line), Token("TEMPLATE_END", line)))
    return out, len(source), line


def _csharp_interpolation_prefix(source: str, start: int) -> tuple[int, bool] | None:
    if source.startswith('$@"', start) or source.startswith('@$"', start):
        return start + 2, True
    if source.startswith('$"', start):
        return start + 1, False
    return None


def _csharp_interpolated_tokens(source: str, start: int, quote: int, line: int, verbatim: bool) -> tuple[list[Token], int, int]:
    out = [Token("INTERPOLATED_START", line)]
    i = quote + 1
    chunk_line = line
    while i < len(source):
        char = source[i]
        if verbatim and source.startswith('""', i):
            i += 2
            continue
        if not verbatim and char == "\\":
            if i + 1 < len(source) and source[i + 1] == "\n":
                line += 1
            i += 2
            continue
        if source.startswith("{{", i) or source.startswith("}}", i):
            i += 2
            continue
        if char == "{":
            close = _template_expression_end(source, i - 1)
            if close >= 0:
                out.append(Token("LIT", chunk_line))
                out.append(Token("INTERPOLATION_START", line))
                expression = source[i + 1 : close]
                expression_line = line + source.count("\n", i + 1, close)
                out.extend(Token(token.value, expression_line + token.line - 1) for token in _generic_tokens(expression, "csharp"))
                expression_end_line = line + source.count("\n", i, close + 1)
                out.append(Token("INTERPOLATION_END", expression_end_line))
                line += source.count("\n", i, close + 1)
                i = close + 1
                chunk_line = line
                continue
        if char == '"':
            out.extend((Token("LIT", chunk_line), Token("INTERPOLATED_END", line)))
            return out, i + 1, line
        if char == "\n":
            line += 1
        i += 1
    out.extend((Token("LIT", chunk_line), Token("INTERPOLATED_END", line)))
    return out, len(source), line


def _generic_tokens(source: str, language: str) -> list[Token]:
    out: list[Token] = []
    i = 0
    line = 1
    words = KEYWORDS[language]
    while i < len(source):
        char = source[i]
        if char.isspace():
            if char == "\n":
                line += 1
            i += 1
            continue
        if source.startswith("//", i):
            end = source.find("\n", i + 2)
            i = len(source) if end < 0 else end
            continue
        if source.startswith("/*", i):
            end = source.find("*/", i + 2)
            stop = len(source) if end < 0 else end + 2
            line += source.count("\n", i, stop)
            i = stop
            continue
        if char == "`" and language in {"javascript", "typescript"}:
            template, i, line = _template_tokens(source, i, line, language)
            out.extend(template)
            continue
        if language == "csharp" and char in "@$":
            prefix = _csharp_interpolation_prefix(source, i)
            if prefix is not None:
                quote, verbatim = prefix
                interpolated, i, line = _csharp_interpolated_tokens(source, i, quote, line, verbatim)
                out.extend(interpolated)
                continue
        if char in "\"'`":
            if language == "rust" and char == "'" and i + 1 < len(source):
                close = source.find("'", i + 1, min(len(source), i + 5))
                if close < 0:
                    out.append(Token("'", line))
                    i += 1
                    continue
            token_line = line
            i, added_lines = _skip_quoted(source, i, char)
            line += added_lines
            out.append(Token("LIT", token_line))
            continue
        number = NUMBER_RE.match(source, i)
        if number and (char.isdigit() or (char == "." and i + 1 < len(source) and source[i + 1].isdigit())):
            token_line = line
            i = number.end()
            out.append(Token("LIT", token_line))
            continue
        js_ident = language in {"javascript", "typescript"} and char == "$"
        if language == "rust" and source.startswith("r#", i) and i + 2 < len(source) and (source[i + 2] == "_" or source[i + 2].isalpha()):
            token_line = line
            i += 3
            while i < len(source) and (source[i] == "_" or source[i].isalnum() or ord(source[i]) >= 128):
                i += 1
            out.append(Token("ID", token_line))
            continue
        if char == "_" or char.isalpha() or ord(char) >= 128 or js_ident:
            token_line = line
            word_start = i
            i += 1
            while i < len(source) and (source[i] == "_" or source[i].isalnum() or ord(source[i]) >= 128 or (language in {"javascript", "typescript"} and source[i] == "$")):
                i += 1
            value = source[word_start:i]
            out.append(Token(value if value in words else "ID", token_line))
            continue
        operator = next((op for op in OPERATORS if source.startswith(op, i)), char)
        out.append(Token(operator, line))
        i += len(operator)
    return out


def _json_tokens(source: str) -> list[Token]:
    """Canonicalise JSON as sorted key paths and container structure, ignoring values.

    A file that is named .json but does not parse (a binary fixture, an empty generated file) has
    no structure to compare, so it yields no tokens instead of failing the whole scan.
    """
    try:
        json.loads(source)
    except ValueError:
        return []
    line_starts = [0]
    line_starts.extend(index + 1 for index, char in enumerate(source) if char == "\n")

    def line_at(offset: int) -> int:
        return bisect.bisect_right(line_starts, offset)

    def whitespace(offset: int) -> int:
        while offset < len(source) and source[offset].isspace():
            offset += 1
        return offset

    def parse(offset: int):
        offset = whitespace(offset)
        if offset >= len(source):
            raise ValueError("unexpected end of JSON")
        line = line_at(offset)
        char = source[offset]
        if char == "{":
            pairs = []
            offset = whitespace(offset + 1)
            if offset < len(source) and source[offset] == "}":
                return ("object", line, pairs), offset + 1
            while True:
                offset = whitespace(offset)
                if offset >= len(source) or source[offset] != '"':
                    raise ValueError(f"expected JSON object key on line {line_at(offset)}")
                key_line = line_at(offset)
                key, offset = json.decoder.scanstring(source, offset + 1, True)
                offset = whitespace(offset)
                if offset >= len(source) or source[offset] != ":":
                    raise ValueError(f"expected ':' after JSON key on line {key_line}")
                value, offset = parse(offset + 1)
                pairs.append((key, key_line, value))
                offset = whitespace(offset)
                if offset < len(source) and source[offset] == "}":
                    return ("object", line, pairs), offset + 1
                if offset >= len(source) or source[offset] != ",":
                    raise ValueError(f"expected ',' in JSON object on line {line_at(offset)}")
                offset += 1
        if char == "[":
            values = []
            offset = whitespace(offset + 1)
            if offset < len(source) and source[offset] == "]":
                return ("array", line, values), offset + 1
            while True:
                value, offset = parse(offset)
                values.append(value)
                offset = whitespace(offset)
                if offset < len(source) and source[offset] == "]":
                    return ("array", line, values), offset + 1
                if offset >= len(source) or source[offset] != ",":
                    raise ValueError(f"expected ',' in JSON array on line {line_at(offset)}")
                offset += 1
        try:
            _, end = json.JSONDecoder().raw_decode(source, offset)
        except json.JSONDecodeError as exc:
            raise ValueError(f"invalid JSON on line {exc.lineno}: {exc.msg}") from exc
        return ("scalar", line, None), end

    root, _ = parse(0)
    out: list[Token] = []

    def emit(node, path: tuple[str, ...]):
        kind, line, contents = node
        if kind == "object":
            out.append(Token("JSON_OBJECT", line))
            for key, key_line, value in sorted(contents, key=lambda entry: entry[0]):
                child = (*path, key)
                out.append(Token("KEY:" + "/".join(child), key_line))
                emit(value, child)
            out.append(Token("END_OBJECT", line))
        elif kind == "array":
            out.append(Token("JSON_ARRAY", line))
            for value in contents:
                out.append(Token("JSON_ITEM", value[1]))
                emit(value, (*path, "*"))
            out.append(Token("END_ARRAY", line))
        else:
            out.append(Token("LIT", line))

    emit(root, ())
    return out


def tokens_for(path: str | Path, source: str) -> list[Token]:
    suffix = Path(path).suffix.lower()
    language = SUPPORTED.get(suffix)
    if language is None:
        return []
    if language == "python":
        return _python_tokens(source)
    if language == "json":
        return _json_tokens(source)
    return _generic_tokens(source, language)


def _token_codes(values: Iterable[str], cache: dict[str, int] | None = None) -> array.array:
    """64-bit code per distinct token value.

    The cache keeps hashing at O(distinct spellings) per file: real corpora
    reuse a few hundred spellings, so blake2b is paid once per spelling.
    """
    if cache is None:
        cache = {}
    codes = array.array("Q")
    get = cache.get
    for value in values:
        code = get(value)
        if code is None:
            digest = hashlib.blake2b(
                value.encode("utf-8", "surrogatepass"), digest_size=8, person=b"spothyg1"
            ).digest()
            code = int.from_bytes(digest, "big")
            cache[value] = code
        codes.append(code)
    return codes


def _gram_hashes(codes: array.array, k: int) -> array.array:
    """Rolling polynomial hash per k-gram: O(tokens), packed 64-bit ints."""
    n = len(codes)
    if n < k:
        return array.array("Q")
    hashes = array.array("Q", bytes(8 * (n - k + 1)))
    rolling = 0
    for index in range(k):
        rolling = (rolling * BASE + codes[index]) & MASK
    hashes[0] = rolling
    power = pow(BASE, k - 1, 1 << 64)
    for end in range(k, n):
        rolling = ((rolling - codes[end - k] * power) * BASE + codes[end]) & MASK
        hashes[end - k + 1] = rolling
    return hashes


def _winnowed_positions(values, window: int) -> list[int]:
    """Winnowing: at least one fingerprint per w-gram window, O(k-grams).

    Accepts any random-access sequence of 64-bit k-gram hashes (array('Q') or
    list); identical content yields identical selections on both sides.
    """
    if not values:
        return []
    width = min(window, len(values))
    minima: deque[int] = deque()
    selected: list[int] = []
    last = -1
    for index, value in enumerate(values):
        while minima and values[minima[-1]] >= value:
            minima.pop()
        minima.append(index)
        first_valid = index - width + 1
        while minima and minima[0] < first_valid:
            minima.popleft()
        if index >= width - 1 and minima[0] != last:
            last = minima[0]
            selected.append(last)
    return selected


_ALL_KEYWORDS = frozenset().union(*KEYWORDS.values())


def _token_is_content(value: str) -> bool:
    """Whether one normalised token value can distinguish a real copy.

    Identifiers (ID) and keywords do; literal placeholders and JSON structure
    markers are scaffolding whose wording is fixed, and punctuation spellings
    carry no names at all.
    """
    if value == "ID" or value.startswith("KEY:"):
        return True
    if value in _ALL_KEYWORDS:
        return True
    if value in LOW_INFO_PLACEHOLDERS:
        return False
    return not all(char in LOW_INFO_PUNCTUATION for char in value)


def window_is_low_information(values: list[str], start: int, k: int) -> bool:
    """Whether the k-gram window at `start` is too generic to identify a copy.

    Dropped classes (both measured on confirmed false positives, see
    LOW_INFO_* notes above):
    1. literal-only windows: no token value is an identifier, keyword,
       "KEY:"-prefixed JSON key or any other content spelling;
    2. period-dominated windows: some period p <= LOW_INFO_MAX_PERIOD repeats
       for at least LOW_INFO_PERIOD_FRACTION of the window's aligned positions
       (values[i] == values[i - p]), as in runs of a single assertion
       statement shape.
    Real copies keep their fingerprints because surrounding non-repeating code
    keeps at least one winnowed window per w-gram span alive.
    """
    end = start + k
    if end > len(values):
        return True
    window = values[start:end]
    literal_only = True
    for value in window:
        if _token_is_content(value):
            literal_only = False
            break
    if literal_only:
        return True
    for period in range(1, LOW_INFO_MAX_PERIOD + 1):
        positions = end - start - period
        if positions <= 0:
            break
        allowed_mismatches = int(positions * (1.0 - LOW_INFO_PERIOD_FRACTION) + 1e-9)
        mismatches = 0
        repeated = True
        for offset in range(positions):
            if window[offset] != window[offset + period]:
                mismatches += 1
                if mismatches > allowed_mismatches:
                    repeated = False
                    break
        if repeated:
            return True
    return False


def _kept_fingerprint_positions(
    tokens: list[Token], gram_hashes: array.array, k: int, window: int
) -> list[int]:
    """Winnowed fingerprint positions that survive the low-information filter."""
    values = [token.value for token in tokens]
    return [
        index
        for index in _winnowed_positions(gram_hashes, window)
        if not window_is_low_information(values, index, k)
    ]


def _spans(
    tokens: list[Token], k: int, window: int, cache: dict[str, int] | None = None
) -> list[tuple[int, int, int, int]]:
    """Winnowed fingerprints as (hash, token position, first line, last line).

    Low-information windows (literal-only or period-dominated) are dropped;
    they cannot identify a copy and only create false positives.
    """
    if len(tokens) < k:
        return []
    hashes = _gram_hashes(_token_codes((token.value for token in tokens), cache), k)
    return [
        (hashes[index], index, tokens[index].line, tokens[index + k - 1].line)
        for index in _kept_fingerprint_positions(tokens, hashes, k, window)
    ]


def fingerprints_for(
    path: str | Path, source: str, k: int = DEFAULT_K, window: int = DEFAULT_W
) -> list[tuple[int, int, int, int]]:
    """Public helper: winnowed fingerprints for one in-memory source file."""
    return _spans(tokens_for(path, source), k, window)


def _selects(
    rel: str,
    suffix: str,
    include: list[str] | None,
    exclude: list[str] | None,
) -> bool:
    """Whether one candidate relative path passes suffix and glob filters."""
    if suffix not in SUPPORTED:
        return False
    if include and not any(fnmatch.fnmatchcase(rel, pattern) for pattern in include):
        return False
    if exclude and any(fnmatch.fnmatchcase(rel, pattern) for pattern in exclude):
        return False
    return True


def _source_files(
    root: Path, include: list[str] | None = None, exclude: list[str] | None = None
) -> Iterable[Path]:
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        rel = path.relative_to(root).as_posix()
        if _selects(rel, path.suffix.lower(), include, exclude):
            yield path


def _git_tracked_source_paths(root: Path) -> list[Path] | None:
    """Paths tracked in HEAD or staged in the index under a git work tree.

    Returns None when git is missing, the directory is not inside a work tree,
    or git fails to list files, so callers can fall back to the file walk.
    """
    try:
        probe = subprocess.run(
            ("git", "-C", str(root), "rev-parse", "--is-inside-work-tree"),
            capture_output=True,
            text=True,
            timeout=30,
        )
        if probe.returncode != 0 or probe.stdout.strip() != "true":
            return None
        listing = subprocess.run(
            ("git", "-C", str(root), "ls-files", "-z", "--cached"),
            capture_output=True,
            timeout=120,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if listing.returncode != 0:
        return None
    return [
        root / rel
        for rel in listing.stdout.decode("utf-8", "replace").split("\0")
        if rel
    ]


def iter_target_files(
    root: Path,
    include: list[str] | None = None,
    exclude: list[str] | None = None,
    all_files: bool = False,
) -> Iterable[Path]:
    """Default scan targets for a repository root.

    Inside a git work tree the target set is `git ls-files --cached`: files
    tracked in HEAD plus files staged for commit. Untracked build output
    (for example generated obj/project.assets.json) then never produces
    coverage or contiguous-region matches. Outside git, the file-system walk
    is used, which keeps explicit scanning of arbitrary non-repository paths
    (the fixture corpus, cache directories) working. all_files=True bypasses
    git and walks the directory, so untracked files (for example a new file
    before `git add`) can be scanned on request.
    """
    root = Path(root).resolve()
    tracked = None if all_files else _git_tracked_source_paths(root)
    if tracked is None:
        yield from _source_files(root, include, exclude)
        return
    for path in tracked:
        rel = path.relative_to(root).as_posix()
        if _selects(rel, path.suffix.lower(), include, exclude):
            yield path


_HEADER = struct.Struct("<8sIIIII")  # magic, format, k, w, long_match, file count
_FILE_ENTRY = struct.Struct("<HI")  # path length in bytes, token count


def build_index(reference: Path, output: Path, k: int = DEFAULT_K, window: int = DEFAULT_W) -> tuple[int, int]:
    """Index the reference corpus once into a compact, hash-sorted fingerprint table.

    Column-major binary layout, gzipped: header, file table, u64 fingerprint
    count, then five columns (u64 hashes sorted ascending, u32 file ids, u32
    positions, u32 first lines, u32 last lines) — 24 bytes per winnowed
    fingerprint instead of one entry per token position for every gram width.
    """
    if k < 2 or window < 1:
        raise ValueError("k must be >= 2 and w must be >= 1")
    reference = reference.resolve()
    paths: list[str] = []
    token_counts: list[int] = []
    hashes = array.array("Q")
    file_ids = array.array("I")
    positions = array.array("I")
    line_starts = array.array("I")
    line_ends = array.array("I")
    token_total = 0
    for path in _source_files(reference):
        source = path.read_text(encoding="utf-8", errors="replace")
        tokens = tokens_for(path, source)
        if not tokens:
            continue
        file_id = len(paths)
        paths.append(path.relative_to(reference).as_posix())
        token_counts.append(len(tokens))
        token_total += len(tokens)
        cache: dict[str, int] = {}
        gram_hashes = _gram_hashes(_token_codes((token.value for token in tokens), cache), k)
        for index in _kept_fingerprint_positions(tokens, gram_hashes, k, window):
            hashes.append(gram_hashes[index])
            file_ids.append(file_id)
            positions.append(index)
            line_starts.append(tokens[index].line)
            line_ends.append(tokens[index + k - 1].line)
    if not paths:
        raise ValueError(f"no supported source files found under {reference}")
    order = sorted(range(len(hashes)), key=hashes.__getitem__)
    sorted_hashes = array.array("Q", map(hashes.__getitem__, order))
    sorted_file_ids = array.array("I", map(file_ids.__getitem__, order))
    sorted_positions = array.array("I", map(positions.__getitem__, order))
    sorted_line_starts = array.array("I", map(line_starts.__getitem__, order))
    sorted_line_ends = array.array("I", map(line_ends.__getitem__, order))
    if sys.byteorder == "big":
        for column in (sorted_hashes, sorted_file_ids, sorted_positions, sorted_line_starts, sorted_line_ends):
            column.byteswap()
    output.parent.mkdir(parents=True, exist_ok=True)
    with gzip.open(output, "wb", compresslevel=6) as stream:
        stream.write(_HEADER.pack(INDEX_MAGIC, FORMAT, k, window, LONG_MATCH, len(paths)))
        for rel, count in zip(paths, token_counts):
            encoded = rel.encode("utf-8")
            stream.write(_FILE_ENTRY.pack(len(encoded), count))
            stream.write(encoded)
        stream.write(struct.pack("<Q", len(sorted_hashes)))
        stream.write(sorted_hashes.tobytes())
        stream.write(sorted_file_ids.tobytes())
        stream.write(sorted_positions.tobytes())
        stream.write(sorted_line_starts.tobytes())
        stream.write(sorted_line_ends.tobytes())
    return len(paths), token_total


class _IndexPrefix(NamedTuple):
    """Parsed index header and file table, plus the fingerprint payload offset."""

    k: int
    w: int
    long_match: int
    paths: list[str]
    token_counts: list[int]
    fingerprint_count: int
    payload_offset: int


class _LoadedIndex:
    """Inverted index over reference fingerprints; the hash column is sorted."""

    __slots__ = (
        "k",
        "w",
        "long_match",
        "paths",
        "token_counts",
        "hashes",
        "file_ids",
        "positions",
        "line_starts",
        "line_ends",
    )

    def lookup(self, digest: int) -> tuple[int, int]:
        """Half-open [lo, hi) range of index entries whose hash equals digest."""
        hashes = self.hashes
        lo = bisect.bisect_left(hashes, digest)
        hi = lo
        total = len(hashes)
        while hi < total and hashes[hi] == digest:
            hi += 1
        return lo, hi


def _parse_prefix(blob: bytes) -> _IndexPrefix:
    try:
        magic, fmt, k, window, long_match, file_count = _HEADER.unpack_from(blob, 0)
        if magic != INDEX_MAGIC or fmt != FORMAT:
            raise ValueError("unsupported hygiene index format")
        offset = _HEADER.size
        paths: list[str] = []
        token_counts: list[int] = []
        for _ in range(file_count):
            length, token_count = _FILE_ENTRY.unpack_from(blob, offset)
            offset += _FILE_ENTRY.size
            paths.append(blob[offset : offset + length].decode("utf-8"))
            token_counts.append(token_count)
            offset += length
        (fingerprint_count,) = struct.unpack_from("<Q", blob, offset)
        offset += 8
    except struct.error as exc:
        raise ValueError("truncated or corrupt hygiene index") from exc
    return _IndexPrefix(k, window, long_match, paths, token_counts, fingerprint_count, offset)


def _column(blob: bytes, offset: int, count: int, typecode: str, item_size: int) -> array.array:
    column = array.array(typecode)
    column.frombytes(blob[offset : offset + item_size * count])
    if sys.byteorder == "big":
        column.byteswap()
    return column


def _load_index(index_path: Path) -> _LoadedIndex:
    with gzip.open(index_path, "rb") as stream:
        blob = stream.read()
    prefix = _parse_prefix(blob)
    count = prefix.fingerprint_count
    if len(blob) < prefix.payload_offset + count * 24:
        raise ValueError("truncated or corrupt hygiene index")
    index = _LoadedIndex()
    index.k, index.w, index.long_match = prefix.k, prefix.w, prefix.long_match
    index.paths, index.token_counts = prefix.paths, prefix.token_counts
    offset = prefix.payload_offset
    index.hashes = _column(blob, offset, count, "Q", 8)
    index.file_ids = _column(blob, offset + 8 * count, count, "I", 4)
    index.positions = _column(blob, offset + 12 * count, count, "I", 4)
    index.line_starts = _column(blob, offset + 16 * count, count, "I", 4)
    index.line_ends = _column(blob, offset + 20 * count, count, "I", 4)
    return index


def index_summary(index_path: Path) -> dict:
    """Format, k, w, long_match, file table and fingerprint count of an index."""
    with gzip.open(index_path, "rb") as stream:
        blob = stream.read()
    prefix = _parse_prefix(blob)
    return {
        "format": FORMAT,
        "k": prefix.k,
        "w": prefix.w,
        "long_match": prefix.long_match,
        "files": [
            {"path": rel, "token_count": count}
            for rel, count in zip(prefix.paths, prefix.token_counts)
        ],
        "fingerprints": prefix.fingerprint_count,
    }


def load_allowlist(path: Path | None) -> list[tuple[str, str]]:
    if path is None:
        return []
    with path.open("rb") as stream:
        document = tomllib.load(stream)
    return load_allowlist_data(document)


def load_allowlist_data(document: dict) -> list[tuple[str, str]]:
    entries = document.get("entries", [])
    if not isinstance(entries, list):
        raise ValueError("allow.toml 'entries' must be an array of tables")
    parsed = []
    for entry in entries:
        if not isinstance(entry, dict):
            raise ValueError("every allow entry must be a table with a path and reason")
        pattern, reason = entry.get("path"), entry.get("reason")
        if not isinstance(pattern, str) or not pattern.strip() or not isinstance(reason, str) or not reason.strip():
            raise ValueError("every allow entry needs a non-empty path and reason")
        parsed.append((pattern, reason))
    return parsed


def _merge_spans(spans: Iterable[tuple[int, int]]) -> list[list[int]]:
    """Merge (first, last) line spans, joining adjacent or overlapping ones."""
    merged: list[list[int]] = []
    for start, end in sorted(spans):
        if merged and start <= merged[-1][1] + 1:
            if end > merged[-1][1]:
                merged[-1][1] = end
        else:
            merged.append([start, end])
    return merged


def scan_targets(
    index_path: Path,
    target_root: Path,
    threshold: float = 0.30,
    include: list[str] | None = None,
    allow: list[tuple[str, str]] | None = None,
    exclude: list[str] | None = None,
    all_files: bool = False,
) -> list[dict]:
    """Scan targets against the winnowed fingerprints held in the inverted index.

    Targets are enumerated via iter_target_files: tracked (or staged) files in
    a git work tree, otherwise every file on disk. Low-information windows
    (literal-only or period-dominated) are skipped on both sides, so
    boilerplate-only shapes cannot produce matches.

    Every target k-gram position is probed once via bisect on the sorted hash
    column, so each match pulls only its own posting range: the cost is
    O(target tokens + posting walk), independent of reference corpus volume.
    Contiguous regions are chained from those matches: matched positions whose
    target and reference offsets advance in lockstep, never gapping more than
    2 * w tokens, form a region of at least long_match tokens.
    """
    if not 0 < threshold <= 1:
        raise ValueError("threshold must be greater than 0 and at most 1")
    index = _load_index(index_path)
    k = index.k
    gap_limit = REGION_GAP_FACTOR * index.w
    target_root = target_root.resolve()
    reports = []
    allow = allow or []
    for path in iter_target_files(target_root, include, exclude, all_files):
        try:
            source = path.read_text(encoding="utf-8", errors="replace")
        except OSError as exc:
            # A tracked or staged path that no longer exists on disk (for
            # example deleted after staging) contributes nothing to scan.
            print(f"hygiene: cannot read {path} ({exc}), skipped", file=sys.stderr)
            continue
        tokens = tokens_for(path, source)
        if len(tokens) < k:
            continue
        values = [token.value for token in tokens]
        gram_hashes = _gram_hashes(_token_codes((token.value for token in tokens), {}), k)
        winnowed = set(_kept_fingerprint_positions(tokens, gram_hashes, k, index.w))
        if not winnowed:
            continue
        matched_target_positions: set[int] = set()
        pairs_by_ref: dict[int, list[tuple[int, int, int, int]]] = {}
        per_ref_counts: dict[int, int] = {}
        per_ref_spans: dict[int, list[tuple[int, int]]] = {}
        for gram_index, gram_hash in enumerate(gram_hashes):
            lo, hi = index.lookup(gram_hash)
            if lo >= hi or window_is_low_information(values, gram_index, k):
                continue
            is_winnowed = gram_index in winnowed
            if is_winnowed:
                matched_target_positions.add(gram_index)
            for entry in range(lo, hi):
                file_id = index.file_ids[entry]
                if file_id not in pairs_by_ref:
                    pairs_by_ref[file_id] = []
                    per_ref_counts[file_id] = 0
                    per_ref_spans[file_id] = []
                pairs_by_ref[file_id].append(
                    (gram_index, index.positions[entry], index.line_starts[entry], index.line_ends[entry])
                )
                if is_winnowed:
                    per_ref_counts[file_id] += 1
                    per_ref_spans[file_id].append((index.line_starts[entry], index.line_ends[entry]))
        coverage = len(matched_target_positions) / len(winnowed)
        contiguous: list[dict] = []
        for file_id, bucket in pairs_by_ref.items():
            bucket.sort()
            ref_path = index.paths[file_id]
            chain: list[tuple[int, int, int, int]] = []

            def flush(current: list[tuple[int, int, int, int]], ref_path: str = ref_path) -> None:
                span = current[-1][0] - current[0][0] + k
                if span >= index.long_match:
                    contiguous.append(
                        {
                            "reference": ref_path,
                            "target_lines": [tokens[current[0][0]].line, tokens[current[-1][0] + k - 1].line],
                            "reference_lines": [current[0][2], current[-1][3]],
                            "tokens": span,
                        }
                    )

            for pair in bucket:
                if chain:
                    delta_target = pair[0] - chain[-1][0]
                    if delta_target <= 0 or delta_target != pair[1] - chain[-1][1] or delta_target > gap_limit:
                        flush(chain)
                        chain = []
                chain.append(pair)
            flush(chain)
        should_report = coverage >= threshold or bool(contiguous)
        if not should_report:
            continue
        relative = path.relative_to(target_root).as_posix()
        allowed = next(((pattern, reason) for pattern, reason in allow if fnmatch.fnmatchcase(relative, pattern)), None)
        if allowed:
            continue
        reports.append(
            {
                "target": relative,
                "coverage": coverage,
                "target_lines": _merge_spans(
                    (tokens[position].line, tokens[position + k - 1].line)
                    for position in matched_target_positions
                ),
                "references": [
                    {
                        "path": index.paths[file_id],
                        "fingerprints": per_ref_counts[file_id],
                        "reference_lines": _merge_spans(per_ref_spans[file_id]),
                    }
                    for file_id in sorted(per_ref_counts)
                ],
                "contiguous_regions": contiguous,
            }
        )
    return reports


def _format_ranges(ranges: list[list[int]]) -> str:
    return ",".join(str(start) if start == end else f"{start}-{end}" for start, end in ranges) or "-"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-index", type=Path, metavar="REF_DIR")
    parser.add_argument("--index", type=Path, required=True)
    parser.add_argument("--target", type=Path, metavar="DIR")
    parser.add_argument("--include", action="append", default=[], metavar="GLOB")
    parser.add_argument(
        "--all-files",
        action="store_true",
        help="walk the directory instead of git ls-files, so untracked files are scanned",
    )
    parser.add_argument("--exclude", action="append", default=[], metavar="GLOB")
    parser.add_argument("--allow", type=Path, default=Path(__file__).with_name("allow.toml"))
    parser.add_argument("--threshold", type=float, default=0.30)
    parser.add_argument("--k", type=int, default=DEFAULT_K)
    parser.add_argument("--w", type=int, default=DEFAULT_W)
    args = parser.parse_args(argv)
    try:
        if args.build_index:
            count, tokens = build_index(args.build_index, args.index, args.k, args.w)
            print(f"Built index: {count} files, {tokens} normalised tokens, {args.index}")
            return 0
        if not args.target:
            parser.error("--target is required when --build-index is omitted")
        allow = load_allowlist(args.allow)
        reports = scan_targets(
            args.index, args.target, args.threshold, args.include or None, allow, args.exclude or None, args.all_files
        )
        for report in reports:
            refs = "; ".join(
                f"{ref['path']} lines {_format_ranges(ref['reference_lines'])}"
                for ref in report["references"]
            ) or ", ".join(sorted({region["reference"] for region in report["contiguous_regions"]}))
            print(f"MATCH {report['target']}: coverage {report['coverage']:.1%}; target lines {_format_ranges(report['target_lines'])}; reference {refs}")
            for region in report["contiguous_regions"]:
                ref_lines = _format_ranges([region["reference_lines"]])
                target_lines = _format_ranges([region["target_lines"]])
                print(f"  contiguous {region['tokens']} tokens: {region['reference']}:{ref_lines} -> target:{target_lines}")
        if not reports:
            print("No source-hygiene matches.")
        return 1 if reports else 0
    except (OSError, ValueError, json.JSONDecodeError, tomllib.TOMLDecodeError) as exc:
        print(f"hygiene: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
