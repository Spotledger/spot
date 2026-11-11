#!/usr/bin/env python3
"""Build a browser shell around one tenant's approved, signed tier B component set.

    python tools/component-shell/build-shell.py --components tenant-set.toml \
        --root-key root-key.json --anchor-set anchor-set.json \
        --revocations revocations.json --as-of 2026-10-26T00:00:00Z \
        --min-anchors-generation 3 --min-revocation-generation 12

The tool reads the set file, verifies every package's signature against the store's **signed** trust
material, and writes a project that references those components statically, together with a
content-addressed manifest of the set and a record of the trust material the build used.

Trust does not come from the set file (a recorded security finding: a build host with write access to it could name its
own key and bundle a package the store would refuse). It comes from three documents, each verified
against the one before it:

``--root-key``
    One root's public key, trusted out of band. It is the only file an operator has to vouch for.

``--anchor-set``
    The store registry's root-signed set: the trusted roots with their validity windows, and the
    publisher certificates those roots issued. Verified by ``--root-key``; a build host that edits a
    certificate, a window or the generation breaks the signature.

``--revocations``
    The store's root-signed revocation list (``spot-revocation-list``, the same document the desktop
    host parses), signed by a root the anchor set names and trusted at ``--as-of``.

``--min-anchors-generation`` and ``--min-revocation-generation``
    Required, no default, no environment fallback. A signature does not prove freshness: an attacker who
    controls the build host's files can replay a genuine, older, correctly signed list. Each document's
    ``generation`` is compared with its pin after the signature verifies; lower is refused
    (``component.anchors.stale``, ``component.revocation.stale``). The release process raises them, never lowers.

``--as-of``
    The instant every validity window and every revocation's ``effective_from`` is judged at. Required:
    a build has no clock of its own, so a rebuild of an old set is judged by the date it was reviewed.

A package is bundled only if its certificate chains to a root in that material, the certificate's
window covers ``--as-of``, neither its publisher nor its key is revoked as of ``--as-of``, its digest
is not on the list, and the signature verifies over the manifest's canonical bytes. Anything else is
refused, and nothing is written.

The set file is TOML and names the shell and the packages, and no trust material at all:

    [shell]
    name = "acme-shell"            # the generated assembly and the manifest's shell name

    [[component]]
    package = "components/acme.rating-1.0.0"

A ``[trust]`` table in a set file is refused in the default mode. The fallback mode exists for a set
that was reviewed before the store published signed material:

    python tools/component-shell/build-shell.py --components reviewed-set.toml --reviewed-set

``--reviewed-set [FILE]`` keeps the old behaviour — the keys and revoked ids in the set file's
``[trust]`` table — only when the set file's SHA-256 is recorded in FILE, which defaults to
``tools/component-shell/reviewed-sets.txt`` (one digest per line). It prints a loud notice, because
what it trusts is a file an editor could have written. The default mode is the signed one.

``package`` is relative to the set file. The trust material is public: keys, ids and signatures,
never a secret. Every refusal names a stable code — the desktop host's where the host has one — so a
reviewer and a test can point at the rule rather than at the wording.
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path
from typing import Any, Final

sys.path.insert(0, str(Path(__file__).resolve().parent))

from component_shell import shell as shell_builder  # noqa: E402
from component_shell import trust as trust_material  # noqa: E402
from component_shell.package import Package, Refused, read_package, verify  # noqa: E402

#: Where the host project lives, relative to the repository root.
HOST_PROJECT_RELATIVE = Path("clients/avalonia/src/Spotledger.Component.Host/Spotledger.Component.Host.csproj")

#: The output directory when ``--output`` is not given.
DEFAULT_OUTPUT = "component-shell-out"

#: The reviewed-set digests file this repository ships, relative to this script.
REVIEWED_SETS_FILE: Final = Path(__file__).resolve().parent / "reviewed-sets.txt"

#: The keys a set file may carry: the shell, its components, and — in the fallback mode only — trust.
SET_MEMBERS: Final = frozenset({"component", "shell", "trust"})

#: The keys each closed table may carry. ``[shell]`` names the shell and nothing else, so a pin an
#: operator writes into it is refused rather than silently ignored.
SHELL_MEMBERS: Final = frozenset({"name"})
TRUST_MEMBERS: Final = frozenset({"keys", "revoked_keys", "revoked_publishers"})


def repository_root(start: Path) -> Path:
    """The nearest directory above ``start`` that holds the component host project."""
    for candidate in (start, *start.parents):
        if (candidate / HOST_PROJECT_RELATIVE).is_file():
            return candidate
    return start


def load_set(path: Path, *, allow_trust: bool) -> tuple[str, dict[str, Any], list[Path]]:
    """Reads a set file: the shell's name, its trust table (fallback mode only) and its packages."""
    with path.open("rb") as handle:
        try:
            document = tomllib.load(handle)
        except tomllib.TOMLDecodeError as error:
            raise SystemExit(f"{path}: not TOML: {error}") from error

    unknown = set(document) - SET_MEMBERS
    if unknown:
        raise SystemExit(f"{path}: no such table as {', '.join(sorted(unknown))}")

    shell_table = document.get("shell")
    if not isinstance(shell_table, dict):
        raise SystemExit(f"{path}: [shell] is required")
    unknown = set(shell_table) - SHELL_MEMBERS
    if unknown:
        # A pin written here would be read from a file the build host controls, which is no pin at
        # all; refusing it is better than ignoring a number an operator believes is enforced.
        raise SystemExit(
            f"{path}: [shell] has no such key as {', '.join(sorted(unknown))}; the generations are "
            "pinned on the command line (--min-anchors-generation, --min-revocation-generation)"
        )
    name = shell_table.get("name")
    if not isinstance(name, str) or re.fullmatch(r"[A-Za-z][A-Za-z0-9_.-]{0,63}", name) is None:
        raise SystemExit(f"{path}: [shell] name is 1 to 64 letters, digits, '.', '_' or '-', starting with a letter")

    components = document.get("component")
    if not isinstance(components, list) or not components:
        raise SystemExit(f"{path}: at least one [[component]] is required")

    packages: list[Path] = []
    for index, entry in enumerate(components):
        if not isinstance(entry, dict) or not isinstance(entry.get("package"), str):
            raise SystemExit(f"{path}: [[component]] #{index + 1} names a package directory")
        unknown = set(entry) - {"package"}
        if unknown:
            raise SystemExit(f"{path}: [[component]] #{index + 1} has no such key as {', '.join(sorted(unknown))}")
        packages.append((path.parent / entry["package"]).resolve())

    trust = document.get("trust")
    if trust is None:
        trust = {}
    if not isinstance(trust, dict):
        raise SystemExit(f"{path}: [trust] is a table")
    unknown = set(trust) - TRUST_MEMBERS
    if unknown:
        raise SystemExit(
            f"{path}: [trust] has no such key as {', '.join(sorted(unknown))}; a set file names keys, "
            "revoked keys and revoked publishers and nothing else"
        )
    if trust and not allow_trust:
        # Recorded security finding: the whole point of the default mode. A set file that names keys is not a trust
        # source, and silently ignoring them would leave the operator believing it is.
        raise Refused(
            trust_material.SET_CARRIES_TRUST,
            f"{path} carries [trust]: in the default mode the trust material comes from --root-key, "
            "--anchor-set and --revocations; a reviewed set file may keep [trust] under --reviewed-set",
        )
    keys = trust.get("keys") or {}
    if not isinstance(keys, dict):
        raise SystemExit(f"{path}: [trust.keys] is a table of key id -> public key")
    for key_id, value in keys.items():
        if (
            not isinstance(value, dict)
            or not isinstance(value.get("public_key"), str)
            or not isinstance(value.get("publisher_id"), str)
            or not value["publisher_id"]
        ):
            raise SystemExit(f"{path}: trust key {key_id} carries a public_key and a publisher_id")
    return name, trust, packages


def verify_set(packages: list[Path], trust: dict[str, Any]) -> list[Package]:
    """The fallback mode's check: every package against the keys the reviewed set file names."""
    keys = {key_id: value for key_id, value in (trust.get("keys") or {}).items()}
    revoked_keys = tuple(trust.get("revoked_keys") or ())
    revoked_publishers = tuple(trust.get("revoked_publishers") or ())

    verified: list[Package] = []
    for directory in packages:
        package = read_package(str(directory))
        verify(package, keys, revoked_keys=revoked_keys, revoked_publishers=revoked_publishers)
        verified.append(package)
    return _without_duplicates(verified)


def verify_against_material(packages: list[Path], material: trust_material.TrustMaterial, as_of: int) -> list[Package]:
    """The default mode's check: every package against the store's verified signed material."""
    verified: list[Package] = []
    for directory in packages:
        package = read_package(str(directory))
        material.check(package, as_of)
        verified.append(package)
    return _without_duplicates(verified)


def _without_duplicates(verified: list[Package]) -> list[Package]:
    """Refuses a set that names one component id twice."""
    identifiers = [package.manifest.component_id for package in verified]
    duplicates = {value for value in identifiers if identifiers.count(value) > 1}
    if duplicates:
        raise Refused(
            "component.set.duplicate",
            "the set lists " + ", ".join(sorted(duplicates)) + " more than once",
        )
    return verified


def _read(path: Path) -> bytes:
    """Reads a file the build was told to use, with a readable refusal when it is not there."""
    try:
        return path.read_bytes()
    except OSError as error:
        raise Refused("component.trust.unreadable", f"{path}: {error.strerror or error}") from error


def _generation(text: str) -> int:
    """A pin: a plain non-negative decimal integer, nothing else."""
    if re.fullmatch(r"[0-9]{1,18}", text) is None:
        raise argparse.ArgumentTypeError(f"{text!r} is not a non-negative integer")
    return int(text)


def build_parser() -> argparse.ArgumentParser:
    """The command line, so ``--help`` is the documentation a build script can read."""
    parser = argparse.ArgumentParser(
        description="Build a browser shell around a signed component set.",
        epilog=(
            "The default mode takes trust from the store's signed material (--root-key, --anchor-set, "
            "--revocations, --as-of); --reviewed-set is the reviewed-set fallback, which trusts the keys "
            "in the set file itself and is refused unless that file's digest is recorded."
        ),
    )
    parser.add_argument("--components", required=True, metavar="FILE", help="the set file (TOML)")
    parser.add_argument("--output", default=DEFAULT_OUTPUT, help=f"the folder to write (default: {DEFAULT_OUTPUT})")
    parser.add_argument("--host-project", default=None, help="the Spotledger.Component.Host project to reference")
    parser.add_argument("--print-manifest", action="store_true", help="also print the manifest to stdout")
    parser.add_argument(
        "--root-key",
        metavar="FILE",
        help="the root public key that signs the anchor set (JSON; trusted out of band)",
    )
    parser.add_argument("--anchor-set", metavar="FILE", help="the store registry's root-signed anchor set (JSON)")
    parser.add_argument("--revocations", metavar="FILE", help="the store's root-signed revocation list (JSON)")
    parser.add_argument(
        "--as-of",
        metavar="INSTANT",
        help="the instant every window and revocation date is judged at: UTC seconds or ISO-8601",
    )
    parser.add_argument(
        "--min-revocation-generation",
        type=_generation,
        metavar="N",
        help=(
            "the lowest revocation-list generation this build accepts (non-negative integer; required in the "
            "default mode, no default, no environment fallback)"
        ),
    )
    parser.add_argument(
        "--min-anchors-generation",
        type=_generation,
        metavar="N",
        help="the lowest anchor-set generation this build accepts (same rules as --min-revocation-generation)",
    )
    parser.add_argument(
        "--reviewed-set",
        nargs="?",
        const=str(REVIEWED_SETS_FILE),
        default=None,
        metavar="FILE",
        help=(
            "fallback mode: trust the set file's own [trust] table, but only when the set file's digest is "
            f"recorded in FILE (default: {REVIEWED_SETS_FILE.relative_to(Path(__file__).resolve().parents[2])})"
        ),
    )
    return parser


def _signed_mode(parser: argparse.ArgumentParser, arguments: argparse.Namespace) -> bool:
    """True for the default mode; fills in and validates whichever mode was asked for."""
    signed = [name for name in ("root_key", "anchor_set", "revocations", "as_of") if getattr(arguments, name)]
    pins = [
        name
        for name in ("min_revocation_generation", "min_anchors_generation")
        if getattr(arguments, name) is not None
    ]
    if arguments.reviewed_set is not None:
        if signed or pins:
            parser.error(
                "--reviewed-set is the fallback mode: do not also pass --root-key, --anchor-set, --revocations, "
                "--as-of, --min-revocation-generation or --min-anchors-generation"
            )
        return False
    missing = [name for name in ("root_key", "anchor_set", "revocations", "as_of") if not getattr(arguments, name)]
    missing += [
        name for name in ("min_revocation_generation", "min_anchors_generation") if getattr(arguments, name) is None
    ]
    if missing:
        parser.error(
            "the default mode needs "
            + ", ".join("--" + name.replace("_", "-") for name in missing)
            + " (or --reviewed-set for a reviewed set file)"
        )
    return True


def main(argv: list[str] | None = None) -> int:
    """Parses the arguments, verifies the set and writes the shell."""
    parser = build_parser()
    arguments = parser.parse_args(argv)
    signed = _signed_mode(parser, arguments)

    set_path = Path(arguments.components).resolve()
    if not set_path.is_file():
        print(f"{set_path}: no such set file", file=sys.stderr)
        return 2

    try:
        name, trust, packages = load_set(set_path, allow_trust=not signed)
    except Refused as refusal:
        print(f"refused: {refusal}", file=sys.stderr)
        return 1

    as_of = 0
    material: trust_material.TrustMaterial | None = None
    set_bytes = set_path.read_bytes()
    reviewed_list_name = ""
    reviewed_list_sha256 = ""
    try:
        if signed:
            as_of = trust_material.parse_instant(arguments.as_of)
            material = trust_material.load_trust_material(
                anchor_set=_read(Path(arguments.anchor_set).resolve()),
                revocations=_read(Path(arguments.revocations).resolve()),
                root_key=_read(Path(arguments.root_key).resolve()),
                as_of=as_of,
                min_anchors_generation=arguments.min_anchors_generation,
                min_revocation_generation=arguments.min_revocation_generation,
            )
            verified = verify_against_material(packages, material, as_of)
        else:
            reviewed_path = Path(arguments.reviewed_set).resolve()
            reviewed_bytes = _read(reviewed_path)
            reviewed_list_name = reviewed_path.name
            reviewed_list_sha256 = trust_material.reviewed_set_digest(reviewed_bytes)
            digest = trust_material.reviewed_set_digest(set_bytes)
            try:
                reviewed = trust_material.parse_reviewed_list(reviewed_bytes.decode("utf-8"))
            except UnicodeDecodeError as error:
                raise Refused(
                    trust_material.REVIEWED_LIST_MALFORMED, f"{reviewed_path} is not UTF-8: {error}"
                ) from error
            trust_material.require_reviewed(digest, reviewed, str(reviewed_path))
            print(
                f"WARNING: reviewed-set mode: {set_path.name} is not verified against the store's signed "
                f"material.\nWARNING: its digest {digest} is recorded in {reviewed_path}.",
                file=sys.stderr,
            )
            print(
                "WARNING: the keys in its [trust] table are trusted because a reviewer read them, not because "
                "the store signed them.",
                file=sys.stderr,
            )
            verified = verify_set(packages, trust)
    except Refused as refusal:
        print(f"refused: {refusal}", file=sys.stderr)
        return 1
    except OSError as error:
        print(f"refused: the package could not be read: {error}", file=sys.stderr)
        return 1

    host_project = (
        Path(arguments.host_project).resolve()
        if arguments.host_project
        else repository_root(Path(__file__).resolve().parent) / HOST_PROJECT_RELATIVE
    )
    if not host_project.is_file():
        print(f"{host_project}: the component host project was not found; pass --host-project", file=sys.stderr)
        return 2

    output = Path(arguments.output).resolve()
    try:
        document = shell_builder.generate(
            shell=name,
            packages=verified,
            output=output,
            host_project=host_project,
        )
        entries = [
            trust_material.entry_of(entry.component_id, entry.component_version, entry.manifest_sha256)
            for entry in document.entries
        ]
        trust_digest = (
            material.digest if material is not None else trust_material.reviewed_set_digest(set_bytes)
        )
        if material is not None:
            record = trust_material.signed_record(
                shell=name, components=entries, set_manifest=document.file_name, material=material,
                as_of=as_of,
                min_anchors_generation=arguments.min_anchors_generation,
                min_revocation_generation=arguments.min_revocation_generation,
            )
        else:
            record = trust_material.reviewed_record(
                shell=name,
                components=entries,
                set_manifest=document.file_name,
                set_sha256=trust_material.reviewed_set_digest(set_bytes),
                reviewed_list=reviewed_list_name,
                reviewed_list_sha256=reviewed_list_sha256,
            )
        record.write(str(output))
    except Refused as refusal:
        print(f"refused: {refusal}", file=sys.stderr)
        return 1
    except OSError as error:
        print(f"refused: the shell could not be written: {error}", file=sys.stderr)
        return 1

    if arguments.print_manifest:
        print(document.text)

    mode = "REVIEWED SET (unverified)"
    if material is not None:
        # The numbers a later audit needs, on the build's own log line: which signed documents this
        # shell was built against, and the pins they had to clear.
        mode = (
            f"signed trust material, anchors generation {material.anchor_set.generation} "
            f"(pinned >= {arguments.min_anchors_generation}), revocation generation "
            f"{material.revocations.generation} (pinned >= {arguments.min_revocation_generation})"
        )
    print(
        f"{output}: {len(document.entries)} component(s), manifest {document.file_name} "
        f"(content {document.sha256[:16]}), record {record.file_name} "
        f"(trust {trust_digest[:16]}, {mode})"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
