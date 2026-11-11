#!/usr/bin/env python3
"""Pinned reference-corpus driver for gate G7 (standard library only).

Downloads the pinned frappe/frappe and frappe/erpnext releases into a cache
outside the repository, fingerprints them into a hygiene index, and measures
seeded-copy recall and the false-positive rate on our own repository.
Reference source is treated as opaque input: nothing in this tool ever prints,
logs or stores reference source text — only counts, paths and line numbers.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import re
import shutil
import sys
import tarfile
import tempfile
import time
import tomllib
import urllib.request
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import hygiene

HYGIENE_DIR = Path(__file__).resolve().parent
REPO_ROOT = HYGIENE_DIR.parents[2]
DEFAULT_PIN = HYGIENE_DIR / "reference.toml"
CACHE_DIR_NAME = "spot-hygiene"
ARCHIVE_SUFFIX = ".tar.gz"
EXTRACT_SUFFIXES = {".py", ".js", ".jsx", ".mjs", ".ts", ".tsx", ".json"}
MIN_RECALL = 0.95
DEFAULT_RECALL_COUNT = 40
RECALL_MIN_TOKENS = 120
FRAGMENT_MIN_TOKENS = 150
# One extracted block. It must be long enough for the scanner's contiguous-region
# path: a winnowed index holds only ~2/(w+1) of a file's k-grams, so a short
# block often matches too sparsely to chain a long_match-token region even
# though every token is verbatim. A few hundred tokens is still a realistic
# "copied function or block" scenario.
FRAGMENT_WINDOW_TOKENS = 480
SCAN_INCLUDE = ["crates/**", "clients/**", "scenarios/**", "harness/**", "tools/**", "lab/**"]
CORPUS_EXCLUDE = ["tools/lints/hygiene/tests/corpus/**"]
TRANSFORM_RENAME = "rename"
TRANSFORM_FRAGMENT = "fragment"
TRANSFORM_JSON = "json-values"
CODE_BUCKETS = {"python": ".py", "javascript": ".js"}
JSON_BUCKET = "json"
HEX_RE = re.compile(r"^[0-9a-f]+$")


class ReferenceError(ValueError):
    """Raised for pin, cache or corpus problems the caller must fix."""


@dataclass(frozen=True)
class PinEntry:
    """One pinned reference snapshot: repo, tag, commit, archive URL and digest."""

    repo: str
    tag: str
    commit: str
    url: str
    sha256: str

    @property
    def slug(self) -> str:
        return self.repo.replace("/", "-")

    @property
    def prefix(self) -> str:
        return self.commit[:8]


def load_pin(pin_path: Path, require_digest: bool = True) -> list[PinEntry]:
    """Read and validate reference.toml. Offline; raises ReferenceError on any defect."""
    try:
        with Path(pin_path).open("rb") as stream:
            document = tomllib.load(stream)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise ReferenceError(f"cannot read pin file {pin_path}: {exc}") from exc
    entries = document.get("references")
    if not isinstance(entries, list) or not entries:
        raise ReferenceError("reference.toml must contain at least one [[references]] table")
    parsed: list[PinEntry] = []
    seen: set[str] = set()
    for item in entries:
        if not isinstance(item, dict):
            raise ReferenceError("every [[references]] entry must be a table")
        required = ("repo", "tag", "commit", "url") + (("sha256",) if require_digest else ())
        missing = [field for field in required if not isinstance(item.get(field), str) or not item[field].strip()]
        if missing:
            raise ReferenceError(f"pin entry for {item.get('repo', '?')} is missing fields: {', '.join(missing)}")
        entry = PinEntry(
            repo=item["repo"].strip(),
            tag=item["tag"].strip(),
            commit=item["commit"].strip().lower(),
            url=item["url"].strip(),
            sha256=item["sha256"].strip().lower(),
        )
        if entry.repo.count("/") != 1 or not all(entry.repo.split("/")):
            raise ReferenceError(f"pin repo must be 'owner/name', got {entry.repo!r}")
        if not HEX_RE.match(entry.commit) or len(entry.commit) != 40:
            raise ReferenceError(f"pin commit for {entry.repo} must be 40 hex characters")
        if require_digest and (not HEX_RE.match(entry.sha256) or len(entry.sha256) != 64):
            raise ReferenceError(f"pin sha256 for {entry.repo} must be 64 hex characters")
        if not require_digest and entry.sha256 and (not HEX_RE.match(entry.sha256) or len(entry.sha256) != 64):
            raise ReferenceError(f"pin sha256 for {entry.repo} must be 64 hex characters")
        if entry.commit not in entry.url:
            raise ReferenceError(f"pin url for {entry.repo} must embed the commit SHA")
        if not entry.url.startswith("https://"):
            raise ReferenceError(f"pin url for {entry.repo} must be https")
        if entry.repo in seen:
            raise ReferenceError(f"duplicate pin entry for {entry.repo}")
        seen.add(entry.repo)
        parsed.append(entry)
    return parsed


def cache_root() -> Path:
    """Cache directory outside the repository: ${XDG_CACHE_HOME:-~/.cache}/spot-hygiene."""
    base = os.environ.get("XDG_CACHE_HOME")
    if base:
        root = Path(base) / CACHE_DIR_NAME
    else:
        root = Path.home() / ".cache" / CACHE_DIR_NAME
    override = os.environ.get("SPOT_HYGIENE_CACHE")
    if override:
        root = Path(override)
    return root


def archive_path(entry: PinEntry, cache: Path) -> Path:
    return cache / "archives" / f"{entry.slug}-{entry.commit}{ARCHIVE_SUFFIX}"


def corpus_dir(entry: PinEntry, cache: Path) -> Path:
    return cache / "corpus" / f"{entry.slug}-{entry.prefix}"


def index_path(entries: list[PinEntry], cache: Path) -> Path:
    stamp = "-".join(entry.prefix for entry in entries)
    return cache / f"index-{stamp}.bin"


def _digest_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def _download(url: str, destination: Path) -> None:
    """Stream the archive to destination, refusing anything but a 200 response."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    partial = destination.with_suffix(destination.suffix + ".part")
    request = urllib.request.Request(url, headers={"User-Agent": "spot-hygiene-pin/1.0"})
    with urllib.request.urlopen(request, timeout=300) as response:
        if response.status != 200:
            raise ReferenceError(f"download of {url} failed with HTTP {response.status}")
        with partial.open("wb") as stream:
            total = 0
            while True:
                block = response.read(1 << 20)
                if not block:
                    break
                stream.write(block)
                total += len(block)
    if total == 0:
        partial.unlink(missing_ok=True)
        raise ReferenceError(f"download of {url} produced an empty body")
    partial.replace(destination)


def _safe_member_name(name: str) -> str | None:
    """Strip the archive's single root directory; return None for anything unsafe."""
    normalised = name.replace("\\", "/")
    if normalised.startswith("/") or ":" in normalised[:3]:
        return None
    parts = [part for part in normalised.split("/") if part not in ("", ".")]
    if not parts or any(part == ".." for part in parts):
        return None
    parts = parts[1:]  # drop the archive root directory (repo-<ref>)
    if not parts or parts[0] == "pax_global_header":
        return None
    return "/".join(parts)


def _is_fingerprinted_file(path: Path) -> bool:
    return path.is_file() and path.suffix.lower() in EXTRACT_SUFFIXES


def _extract_archive(entry: PinEntry, destination: Path, extracted_dir: Path) -> int:
    extracted_dir.mkdir(parents=True, exist_ok=True)
    count = 0
    with tarfile.open(destination, "r:gz") as archive:
        for member in archive.getmembers():
            if not member.isreg():
                continue
            name = _safe_member_name(member.name)
            if name is None:
                continue
            suffix = Path(name).suffix.lower()
            if suffix not in EXTRACT_SUFFIXES:
                continue
            # _safe_member_name rejected "..", absolute and drive-qualified names,
            # so target always stays inside extracted_dir.
            target = extracted_dir / name
            target.parent.mkdir(parents=True, exist_ok=True)
            source_file = archive.extractfile(member)
            if source_file is None:
                continue
            with source_file, open(target, "wb") as output:
                shutil.copyfileobj(source_file, output)
            count += 1
    return count


def fetch(entries: list[PinEntry], cache: Path) -> list[int]:
    """Download, digest-verify and extract the pinned archives. Returns extracted file counts."""
    counts: list[int] = []
    for entry in entries:
        destination = archive_path(entry, cache)
        if destination.exists():
            actual = _digest_file(destination)
            if actual != entry.sha256:
                destination.unlink()
                raise ReferenceError(
                    f"cached archive for {entry.repo} {entry.tag} does not match the pinned sha256; removed"
                )
            print(f"archive {entry.repo} {entry.tag}: cached, digest verified")
        else:
            started = time.perf_counter()
            _download(entry.url, destination)
            elapsed = time.perf_counter() - started
            actual = _digest_file(destination)
            if actual != entry.sha256:
                destination.unlink(missing_ok=True)
                raise ReferenceError(
                    f"archive for {entry.repo} {entry.tag} has sha256 {actual}, pin says {entry.sha256}"
                )
            print(f"archive {entry.repo} {entry.tag}: downloaded in {elapsed:.1f}s, digest verified")
        extracted_dir = corpus_dir(entry, cache)
        if extracted_dir.is_dir() and any(_ for _ in extracted_dir.iterdir() if _is_fingerprinted_file(_)):
            count = sum(1 for path in extracted_dir.rglob("*") if _is_fingerprinted_file(path))
            print(f"corpus {entry.repo} {entry.tag}: already extracted ({count} files)")
            counts.append(count)
            continue
        count = _extract_archive(entry, destination, extracted_dir)
        if count < 100:
            raise ReferenceError(f"corpus for {entry.repo} extracted only {count} files; archive looks wrong")
        counts.append(count)
        print(f"corpus {entry.repo} {entry.tag}: extracted {count} fingerprinted files")
    return counts


def print_digests(entries: list[PinEntry], cache: Path) -> None:
    """Pin bootstrap helper: download (or reuse) archives and print their sha256 only.

    Used to fill reference.toml when bumping the pin; never extracts anything.
    """
    for entry in entries:
        destination = archive_path(entry, cache)
        if not destination.exists():
            _download(entry.url, destination)
        print(f"{entry.repo} {entry.tag} sha256: {_digest_file(destination)}")


def build(entries: list[PinEntry], cache: Path, output: Path | None = None) -> tuple[Path, dict]:
    """Build one hygiene index over every extracted pinned corpus."""
    corpus = cache / "corpus"
    if not corpus.is_dir():
        raise ReferenceError(f"no extracted corpus under {corpus}; run --fetch first")
    for entry in entries:
        extracted = corpus_dir(entry, cache)
        if not extracted.is_dir() or not any(extracted.iterdir()):
            raise ReferenceError(f"corpus for {entry.repo} is missing under {extracted}; run --fetch first")
    destination = output or index_path(entries, cache)
    started = time.perf_counter()
    file_count, token_count = hygiene.build_index(corpus, destination)
    elapsed = time.perf_counter() - started
    size = destination.stat().st_size
    peak_kib = 0
    try:
        import resource

        peak_kib = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    except ImportError:  # pragma: no cover - non-POSIX
        pass
    print(
        f"Built {destination.name}: {file_count} files, {token_count} tokens, "
        f"{size / 1e6:.1f} MB, built in {elapsed:.1f}s, peak RSS {peak_kib / 1024:.0f} MiB"
    )
    print(destination)
    return destination, {"files": file_count, "tokens": token_count, "size": size, "seconds": elapsed}


_IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def _rename_identifiers(source: str, language: str) -> str:
    """Deterministically rename non-keyword identifiers; keywords stay keywords."""
    reserved = hygiene.KEYWORDS[language]
    mapping: dict[str, str] = {}
    counter = 0
    pieces: list[str] = []
    cursor = 0
    for match in _IDENT_RE.finditer(source):
        pieces.append(source[cursor : match.start()])
        word = match.group(0)
        replacement = mapping.get(word)
        if replacement is None:
            if word in reserved:
                replacement = word
            else:
                replacement = f"sym_{counter:04d}"
                counter += 1
                mapping[word] = replacement
        pieces.append(replacement)
        cursor = match.end()
    pieces.append(source[cursor:])
    return "".join(pieces)


def _reformat_python(source: str) -> str:
    """Whitespace-only reformat: tab expansion and narrower, unit-preserving indents."""
    out = []
    for line in source.splitlines():
        line = line.rstrip()
        if not line.strip():
            out.append("")
            continue
        indent = re.match(r"[ \t]*", line).group(0)
        unit = len(indent.replace("\t", "    ")) // 4
        out.append("  " * unit + line[len(indent) :])
    return "\n".join(out) + "\n"


def _reformat_javascript(source: str) -> str:
    out = []
    for line in source.splitlines():
        line = line.replace("\t", "  ").rstrip()
        if line.strip():
            out.append(line)
    return "\n".join(out) + "\n"


def _rename_transform(source: str, language: str) -> str:
    renamed = _rename_identifiers(source, language)
    if language == "python":
        return _reformat_python(renamed)
    return _reformat_javascript(renamed)


def _contains_run(haystack: list[str], needle: list[str]) -> bool:
    """Whether needle appears in haystack as one contiguous run."""
    if not needle or len(needle) > len(haystack):
        return False
    first = needle[0]
    for index in range(len(haystack) - len(needle) + 1):
        if haystack[index] == first and haystack[index : index + len(needle)] == needle:
            return True
    return False


def _wrap_fragment(chunk: str, suffix: str) -> str:
    if suffix == ".py":
        indented = "\n".join(("    " + line) if line.strip() else "" for line in chunk.splitlines())
        return (
            "def unrelated_wrapper():\n"
            "    try:\n"
            f"{indented}\n"
            "    except Exception:\n"
            "        pass\n"
        )
    return (
        "export function unrelatedWrapper() {\n"
        "  try {\n"
        + "\n".join(chunk.splitlines())
        + "\n  } catch (probeError) {\n"
        "    return null;\n"
        "  }\n"
        "}\n"
    )


def _fragment_transform(source: str, suffix: str) -> str:
    """Extract one token-dense window of source into an unrelated wrapper file.

    The chunk must re-tokenise to the same token values as in the original: a
    chunk that starts mid string, template or bracket statement would not, so
    its k-gram hashes would not exist in the reference file and no chain could
    form. Slide the window to the next line boundary until the extraction is
    clean; real copies are syntactically bounded blocks, so this keeps the
    seeded copy realistic rather than easier.
    """
    tokens = hygiene.tokens_for(f"probe{suffix}", source)
    if len(tokens) < RECALL_MIN_TOKENS:
        raise ReferenceError(f"fragment transform needs at least {RECALL_MIN_TOKENS} tokens, got {len(tokens)}")
    start = len(tokens) // 3 if len(tokens) > FRAGMENT_WINDOW_TOKENS + 40 else 0
    lines = source.splitlines()
    while True:
        window = tokens[start : start + FRAGMENT_WINDOW_TOKENS]
        if len(window) < FRAGMENT_MIN_TOKENS:
            raise ReferenceError("no clean fragment window in this file")
        first_line = window[0].line
        last_line = window[-1].line
        chunk = "\n".join(lines[first_line - 1 : last_line])
        probe = f"probe{suffix}"
        window_values = [token.value for token in window]
        chunk_values = [token.value for token in hygiene.tokens_for(probe, chunk)]
        if _contains_run(chunk_values, window_values):
            return _wrap_fragment(chunk, suffix)
        following = first_line + 1
        start = next((index for index in range(start, len(tokens)) if tokens[index].line >= following), None)
        if start is None:
            raise ReferenceError("no clean fragment window in this file")


def _json_values_transform(source: str) -> str:
    """Same keys, same structure, programmatically changed scalar values."""
    document = json.loads(source)
    counter = {"n": 0}

    def replace(value):
        counter["n"] += 1
        if isinstance(value, dict):
            return {key: replace(item) for key, item in value.items()}
        if isinstance(value, list):
            return [replace(item) for item in value]
        if isinstance(value, bool):
            return not value
        if isinstance(value, int):
            return (value * 7 + counter["n"]) % 1_000_000
        if isinstance(value, float):
            return round((value * 3 + counter["n"]) % 10_000, 6)
        if isinstance(value, str):
            return f"changed-{counter['n']}"
        return value

    return json.dumps(replace(document), indent=4, ensure_ascii=False) + "\n"


def seed_from_entries(entries: list[PinEntry]) -> int:
    """Deterministic RNG seed derived only from the pinned commit SHAs."""
    material = "".join(entry.commit for entry in sorted(entries, key=lambda item: item.repo))
    return int.from_bytes(hashlib.sha256(material.encode()).digest()[:8], "big")


def _bucket_for(suffix: str) -> str | None:
    if suffix == ".py":
        return "python"
    if suffix in (".js", ".jsx", ".mjs"):
        return "javascript"
    if suffix == ".json":
        return JSON_BUCKET
    return None


def select_reference_files(corpus_root: Path, count: int, seed: int) -> list[tuple[Path, str, str]]:
    """Deterministically pick count corpus files, split across Python, JS and JSON.

    Each bucket is walked in a seeded shuffle order until its quota is filled: a
    candidate below the bucket's token floor is skipped and the walk retries with
    the next one, so the requested count is met even when some candidates are too
    short to transform (same seed, same files — the pin SHAs fix the selection).
    """
    buckets: dict[str, list[tuple[str, Path]]] = {"python": [], "javascript": [], JSON_BUCKET: []}
    for path in sorted(corpus_root.rglob("*")):
        if not _is_fingerprinted_file(path):
            continue
        bucket = _bucket_for(path.suffix.lower())
        if bucket:
            buckets[bucket].append((path.relative_to(corpus_root).as_posix(), path))
    quotas = {"python": round(count * 0.5), "javascript": round(count * 0.25), JSON_BUCKET: 0}
    quotas[JSON_BUCKET] = count - quotas["python"] - quotas["javascript"]
    for bucket, quota in quotas.items():
        quotas[bucket] = min(quota, len(buckets[bucket]))
    leftover = count - sum(quotas.values())
    for bucket in ("python", "javascript", JSON_BUCKET):
        if leftover <= 0:
            break
        extra = min(leftover, len(buckets[bucket]) - quotas[bucket])
        if extra > 0:
            quotas[bucket] += extra
            leftover -= extra
    rng = random.Random(seed)
    chosen: list[tuple[Path, str, str]] = []
    walks: dict[str, tuple[list[tuple[str, Path]], list[int], int, int]] = {}
    for bucket in ("python", "javascript", JSON_BUCKET):
        quota = quotas[bucket]
        if quota <= 0:
            continue
        candidates = buckets[bucket]
        order = list(range(len(candidates)))
        rng.shuffle(order)
        floor = RECALL_MIN_TOKENS if bucket == JSON_BUCKET else FRAGMENT_WINDOW_TOKENS
        picked = 0
        cursor = 0
        while picked < quota and cursor < len(order):
            rel, path = candidates[order[cursor]]
            cursor += 1
            source = path.read_text(encoding="utf-8", errors="replace")
            # Code files must be long enough for either transform half; JSON
            # files only need enough tokens for a meaningful values-only copy.
            if len(hygiene.tokens_for(f"probe{path.suffix}", source)) < floor:
                continue
            chosen.append((path, rel, bucket))
            picked += 1
        walks[bucket] = (candidates, order, cursor, floor)
    # Retry pass: if some bucket could not meet its quota with eligible files,
    # top the request up from the other shuffled walks (deterministic order).
    shortfall = count - len(chosen)
    for bucket in ("python", "javascript", JSON_BUCKET):
        if shortfall <= 0:
            break
        if bucket not in walks:
            continue
        candidates, order, cursor, floor = walks[bucket]
        while shortfall > 0 and cursor < len(order):
            rel, path = candidates[order[cursor]]
            cursor += 1
            source = path.read_text(encoding="utf-8", errors="replace")
            if len(hygiene.tokens_for(f"probe{path.suffix}", source)) < floor:
                continue
            chosen.append((path, rel, bucket))
            shortfall -= 1
    return chosen


def run_seeded_recall(
    index_path: Path,
    corpus_root: Path,
    entries: list[PinEntry],
    count: int,
    threshold: float = 0.30,
    work_dir: Path | None = None,
) -> tuple[dict, int]:
    """Seeded-copy recall on the real corpus. Prints counts only; never file names or source.

    Returns (tally keyed by (transformation, bucket) -> [detected, total], exit code).
    """
    if not index_path.exists():
        raise ReferenceError(f"index {index_path} not found; run --build first")
    selection = select_reference_files(corpus_root, count, seed_from_entries(entries))
    artifacts: dict[str, tuple[str, str, str]] = {}
    owned_temp = work_dir is None
    temp = Path(work_dir or tempfile.mkdtemp(prefix="spot-hygiene-recall-"))
    try:
        for position, (path, rel, bucket) in enumerate(selection):
            source = path.read_text(encoding="utf-8", errors="replace")
            tokens = hygiene.tokens_for(f"probe{path.suffix}", source)
            if bucket == JSON_BUCKET:
                transform = TRANSFORM_JSON
                output = _json_values_transform(source)
            elif (
                position % 2 == 1 and len(tokens) >= FRAGMENT_WINDOW_TOKENS
            ):  # too-short files cannot yield a fragment window; rename them instead
                transform = TRANSFORM_FRAGMENT
                output = _fragment_transform(source, path.suffix)
            else:
                transform = TRANSFORM_RENAME
                output = _rename_transform(source, bucket)
            name = f"{position:03d}-{path.stem}{path.suffix}"
            (temp / name).write_text(output, encoding="utf-8")
            artifacts[name] = (rel, transform, bucket)
        reports = hygiene.scan_targets(index_path, temp, threshold=threshold)
    finally:
        if owned_temp:
            shutil.rmtree(temp, ignore_errors=True)
    return _tally(artifacts, reports)


def _tally(artifacts: dict[str, tuple[str, str, str]], reports: list[dict]) -> tuple[dict, int]:
    report_by_target = {report["target"]: report for report in reports}
    tally: dict[tuple[str, str], list[int]] = {}
    for name, (origin, transform, bucket) in artifacts.items():
        key = (transform, bucket)
        tally.setdefault(key, [0, 0])[1] += 1
        report = report_by_target.get(name)
        if not report:
            continue
        cited = {ref["path"] for ref in report["references"]}
        cited |= {region["reference"] for region in report["contiguous_regions"]}
        if origin in cited:
            tally[key][0] += 1
    for (transform, bucket), (detected, total) in sorted(tally.items()):
        print(f"recall {transform} {bucket}: {detected}/{total}")
    per_transform: dict[str, list[int]] = {}
    for (transform, _bucket), (detected, total) in tally.items():
        cell = per_transform.setdefault(transform, [0, 0])
        cell[0] += detected
        cell[1] += total
    failed = False
    for transform, (detected, total) in sorted(per_transform.items()):
        rate = detected / total if total else 0.0
        print(f"recall {transform}: {detected}/{total} ({rate:.1%})")
        if total and rate < MIN_RECALL:
            failed = True
    overall_detected = sum(cell[0] for cell in per_transform.values())
    overall_total = sum(cell[1] for cell in per_transform.values())
    print(f"recall overall: {overall_detected}/{overall_total}")
    return tally, (1 if failed else 0)


def run_false_positives(
    index_path: Path, repo_root: Path, allow_path: Path, threshold: float = 0.30
) -> tuple[int, list[dict]]:
    """Scan our repository with the real index. Prints flagged paths and line numbers only."""
    scanned = sum(
        1 for _ in hygiene.iter_target_files(repo_root.resolve(), SCAN_INCLUDE, CORPUS_EXCLUDE)
    )
    allow = hygiene.load_allowlist(allow_path)
    reports = hygiene.scan_targets(
        index_path,
        repo_root.resolve(),
        threshold=threshold,
        include=SCAN_INCLUDE,
        exclude=CORPUS_EXCLUDE,
        allow=allow,
    )
    for report in reports:
        refs = "; ".join(
            f"{ref['path']} lines {hygiene._format_ranges(ref['reference_lines'])}"
            for ref in report["references"]
        ) or ", ".join(sorted({region["reference"] for region in report["contiguous_regions"]}))
        print(
            f"MATCH {report['target']}: coverage {report['coverage']:.1%}; "
            f"target lines {hygiene._format_ranges(report['target_lines'])}; reference {refs}"
        )
    rate = (len(reports) / scanned) if scanned else 0.0
    print(f"scanned {scanned} files, flagged {len(reports)} ({rate:.2%})")
    return scanned, reports


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pin", type=Path, default=DEFAULT_PIN)
    parser.add_argument("--cache", type=Path, default=None, metavar="DIR")
    parser.add_argument("--check-pin", action="store_true", help="validate the pin offline")
    parser.add_argument("--print-digests", action="store_true", help="pin bootstrap: print archive sha256 values")
    parser.add_argument("--fetch", action="store_true", help="download, verify and extract the pinned archives")
    parser.add_argument("--build", action="store_true", help="build the hygiene index from the extracted corpus")
    parser.add_argument("--index", type=Path, default=None, metavar="PATH")
    parser.add_argument(
        "--print-index-path", action="store_true", help="print the expected index path (unconditionally; the caller checks existence)"
    )
    parser.add_argument("--seeded-recall", type=int, nargs="?", const=DEFAULT_RECALL_COUNT, default=None, metavar="N")
    parser.add_argument("--false-positives", action="store_true", help="scan this repository and report the rate")
    parser.add_argument("--threshold", type=float, default=0.30)
    args = parser.parse_args(argv)
    actions = (
        args.check_pin,
        args.print_digests,
        args.fetch,
        args.build,
        args.print_index_path,
        args.seeded_recall is not None,
        args.false_positives,
    )
    if not any(actions):
        parser.error("choose an action: --check-pin, --print-digests, --fetch, --build, --print-index-path, --seeded-recall, --false-positives")
    try:
        cache = args.cache or cache_root()
        entries = load_pin(args.pin, require_digest=not args.print_digests)
        if args.check_pin:
            names = ", ".join(f"{entry.repo} {entry.tag}" for entry in entries)
            print(f"pin OK: {len(entries)} references ({names})")
        if args.print_digests:
            print_digests(entries, cache)
        if args.fetch:
            fetch(entries, cache)
        index = args.index or index_path(entries, cache)
        if args.build:
            build(entries, cache, args.index)
        if args.print_index_path:
            # Print unconditionally and exit 0: the caller (CI) checks whether
            # the index exists and decides whether to run --fetch --build. The
            # path must be printable on a cold cache, before anything is built.
            print(index)
        exit_code = 0
        if args.seeded_recall is not None:
            corpus = cache / "corpus"
            if not corpus.is_dir():
                raise ReferenceError(f"no extracted corpus under {corpus}; run --fetch first")
            _recall_tally, code = run_seeded_recall(
                index, corpus, entries, args.seeded_recall, threshold=args.threshold
            )
            if code:
                exit_code = code
        if args.false_positives:
            if not index.exists():
                raise ReferenceError(f"index {index} not found; run --fetch and --build first")
            _scanned, reports = run_false_positives(
                index, REPO_ROOT, HYGIENE_DIR / "allow.toml", threshold=args.threshold
            )
            if reports:
                exit_code = 1
        return exit_code
    except (ReferenceError, OSError, json.JSONDecodeError) as exc:
        print(f"reference: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())