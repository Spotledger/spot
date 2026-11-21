"""Output writers.

Every table streams into an output file that keeps a running SHA-256
digest and row count; the manifest lists one entry per written file. All
written bytes use LF line endings on every platform. JSONL writes one
line per row with nested children inline. CSV writes RFC 4180 files with
LF line endings: nested children become tables of their own (see
tables.py) and booleans/ints are rendered so the verifier and the tests
read them back typed.
"""

import csv
import hashlib
import io
import json
from pathlib import Path

from . import tables


def dumps_row(obj) -> str:
    """Encode one JSON row deterministically: sorted keys, compact."""
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def write_manifest(path: Path, manifest: dict) -> None:
    text = json.dumps(manifest, sort_keys=True, indent=1) + "\n"
    path.write_text(text, encoding="utf-8", newline="\n")


class JsonlFile:
    """A streaming JSONL output file: rows as they come, plus stats.

    Opened with ``newline=\"\\n\"`` so the bytes on disk keep LF endings on
    every platform; Windows' text mode would otherwise emit CRLF and break
    the SHA-256 digests recorded in ``manifest.json``.
    """

    def __init__(self, path: Path):
        self._file = open(path, "w", encoding="utf-8", newline="\n")
        self._digest = hashlib.sha256()
        self.rows = 0

    def write_row(self, obj) -> None:
        text = dumps_row(obj) + "\n"
        self._file.write(text)
        self._digest.update(text.encode("utf-8"))
        self.rows += 1

    def finish(self) -> dict:
        self._file.close()
        return {"rows": self.rows, "sha256": self._digest.hexdigest()}

    def __enter__(self):
        return self

    def __exit__(self, *_):
        if not self._file.closed:
            self._file.close()


def _csv_cell(value, kind: str) -> str:
    """Render one output value so that the CSV cell round-trips typed."""
    if kind == "bool":
        if value is True:
            return "true"
        if value is False:
            return "false"
        raise TypeError(f"expected a bool column value, got {value!r}")
    if kind == "int":
        if type(value) is int:
            return str(value)
        raise TypeError(f"expected an int column value, got {value!r}")
    if not isinstance(value, str):
        raise TypeError(f"expected a str column value, got {value!r}")
    return value


class CsvFile:
    """A streaming RFC 4180 CSV file for one table's declared columns.

    The header is written once. Only declared columns are projected from
    each row, so a JSONL-shaped row written to CSV keeps exactly the same
    scalars; nested children travel through their own CsvFile. Bare LF line
    endings; QUOTE_MINIMAL escaping; the digest covers every written byte
    including the header, the row count only data rows.
    """

    def __init__(self, path: Path, columns):
        self._file = open(path, "w", encoding="utf-8", newline="")
        self._columns = tuple(columns)
        self._digest = hashlib.sha256()
        self.rows = 0
        self._write_line([name for name, _ in self._columns])

    def _write_line(self, values) -> None:
        buffer = io.StringIO()
        csv.writer(buffer, lineterminator="\n").writerow(values)
        data = buffer.getvalue()
        self._file.write(data)
        self._digest.update(data.encode("utf-8"))

    def write_row(self, obj) -> None:
        values = []
        for name, kind in self._columns:
            if name not in obj:
                raise KeyError(f"missing column {name!r}")
            values.append(_csv_cell(obj[name], kind))
        self._write_line(values)
        self.rows += 1

    def finish(self) -> dict:
        self._file.close()
        return {"rows": self.rows, "sha256": self._digest.hexdigest()}

    def __enter__(self):
        return self

    def __exit__(self, *_):
        if not self._file.closed:
            self._file.close()


class RowSink:
    """Writes rows for every table, splitting CSV children into child files.

    JSONL keeps nested children inline on the parent row (the "lines" /
    "allocations" field of the table schema). CSV writes the parent row
    into its own file and each child line into the child table, keyed by
    the parent id and numbered from 1, in row order.
    """

    def __init__(self, directory: Path, fmt: str):
        self._directory = directory
        self._fmt = fmt
        self._suffix = ".jsonl" if fmt == "jsonl" else ".csv"
        self._files = {}
        for name, table in tables.TABLES.items():
            self._files[name] = self._open(name, table["columns"])
            child = table["child"]
            if fmt == "csv" and child is not None:
                self._files[child["name"]] = self._open(
                    child["name"], tables.full_child_columns(name)
                )

    def _open(self, stem: str, columns):
        path = self._directory / f"{stem}{self._suffix}"
        if self._fmt == "jsonl":
            return JsonlFile(path)
        return CsvFile(path, columns)

    def write(self, stem: str, row: dict) -> None:
        table = tables.TABLES[stem]
        self._files[stem].write_row(row)
        child = table["child"]
        if self._fmt == "csv" and child is not None:
            key = tables.child_key(stem)
            parent_id = row[table["columns"][0][0]]
            for number, line in enumerate(row[table["lines_field"]], 1):
                self._files[child["name"]].write_row(
                    {**line, key: parent_id, "line": number}
                )

    def finish(self) -> dict:
        return {
            f"{name}{self._suffix}": handler.finish()
            for name, handler in self._files.items()
        }

    def __enter__(self):
        return self

    def __exit__(self, *_):
        for handler in self._files.values():
            if not handler._file.closed:
                handler._file.close()