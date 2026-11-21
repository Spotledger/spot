#!/usr/bin/env python3
"""Check the Linux packaging without root, a VM or a network.

    check.py unit [PATH] [--threshold N] [--no-systemd-analyze]
    check.py tarball FILE
    check.py rpm FILE [--version X.Y.Z]
    check.py deb FILE [--version X.Y.Z]
    check.py containerfile [PATH]
    check.py container IMAGE

``unit`` always runs a built-in lint of the unit text; when ``systemd-analyze``
is present it also runs ``verify`` (on a copy whose ExecStart points at an
existing executable, so a host without /usr/bin/spot is not a unit error) and
``security``, failing below the stated exposure threshold. ``rpm`` and ``deb``
read metadata with ``rpm``/``dpkg-deb`` and skip cleanly when those tools are
absent. ``tarball`` needs only ``tarfile``. Every command exits 0 on success or
a clean skip, 1 on a real failure.

Standard library only (Python 3.11).
"""

from __future__ import annotations

import argparse
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

PACKAGING = Path(__file__).resolve().parents[2] / "packaging" / "linux"
DEFAULT_UNIT = PACKAGING / "systemd" / "spot.service"
DEFAULT_CONTAINERFILE = PACKAGING / "container" / "Containerfile"
SECURITY_THRESHOLD = 3.0

ALLOWED_WRITE_PATHS = ("/var/lib/spot", "/run/spot", "/var/log/spot")
ALLOWED_PACKAGE_PREFIXES = ("/usr/", "/etc/", "/var/", "/run/")
BOOLEAN_DIRECTIVES = (
    "NoNewPrivileges",
    "PrivateTmp",
    "PrivateDevices",
    "ProtectKernelTunables",
    "ProtectKernelModules",
    "ProtectKernelLogs",
    "ProtectControlGroups",
    "RestrictNamespaces",
    "LockPersonality",
)
REQUIRED_DIRECTORIES = ("StateDirectory", "ConfigurationDirectory", "LogsDirectory", "RuntimeDirectory")

_DURATION = re.compile(r"^(\d+(?:\.\d+)?)(us|ms|s|min|m|h|d)?$")
_UNIT_SECONDS = {"us": 1e-6, "ms": 1e-3, "s": 1, "": 1, "min": 60, "m": 60, "h": 3600, "d": 86400}


class CheckError(Exception):
    """A packaging check failed."""


def parse_unit(text: str) -> dict[str, dict[str, list[str]]]:
    """Parse ini-style systemd unit text into section -> key -> values."""
    sections: dict[str, dict[str, list[str]]] = {}
    current: str | None = None
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or line.startswith(";"):
            continue
        if line.startswith("[") and line.endswith("]"):
            current = line[1:-1].strip()
            sections.setdefault(current, {})
            continue
        if "=" in line and current is not None:
            key, _, value = line.partition("=")
            sections[current].setdefault(key.strip(), []).append(value.strip())
    return sections


def _truthy(value: str) -> bool:
    return value.strip().lower() in {"1", "yes", "true", "on"}


def _duration(value: str) -> float | None:
    match = _DURATION.match(value.strip())
    if not match:
        return None
    return float(match.group(1)) * _UNIT_SECONDS[match.group(2) or ""]


def _last(section: dict[str, list[str]], key: str) -> str | None:
    values = section.get(key)
    return values[-1] if values else None


def _inside(path: str, prefixes: tuple[str, ...]) -> bool:
    clean = path.rstrip("/") or "/"
    # The prefixes are written with a trailing slash ("/usr/"): compare without it, or no path
    # below one would ever match (the deb check refused every file of a correct package).
    return any(clean == p.rstrip("/") or clean.startswith(p.rstrip("/") + "/") for p in prefixes)


def lint_unit(text: str) -> list[str]:
    """Return every problem found by the built-in unit lint (empty means good)."""
    sections = parse_unit(text)
    service = sections.get("Service", {})
    unit = sections.get("Unit", {})
    install = sections.get("Install", {})
    errors: list[str] = []

    if not service:
        return ["missing [Service] section"]
    if not install:
        errors.append("missing [Install] section")
    elif not _last(install, "WantedBy"):
        errors.append("[Install] has no WantedBy=")

    user = _last(service, "User")
    if user is None:
        errors.append("missing User= (the service must run as spot)")
    elif user.strip() in {"root", "0"} or user.strip() == "":
        errors.append("User=root is forbidden; the service runs as spot")
    group = _last(service, "Group")
    if group is None or group.strip() in {"root", "0", ""}:
        errors.append("Group= must be a dedicated non-root group (spot)")

    unit_type = _last(service, "Type")
    if unit_type not in {"simple", "notify", "exec"}:
        errors.append("Type= must be simple or notify")

    exec_start = _last(service, "ExecStart")
    if not exec_start:
        errors.append("missing ExecStart=")
    elif not exec_start.split()[0].startswith("/"):
        errors.append("ExecStart= must use an absolute binary path")

    for directive in BOOLEAN_DIRECTIVES:
        value = _last(service, directive)
        if value is None:
            errors.append(f"missing {directive}=true (sandbox)")
        elif not _truthy(value):
            errors.append(f"{directive}= must be true, got {value!r}")

    protect_system = _last(service, "ProtectSystem")
    if protect_system is None:
        errors.append("missing ProtectSystem=strict (sandbox)")
    elif protect_system.strip().lower() != "strict":
        errors.append(f"ProtectSystem= must be strict, got {protect_system!r}")

    protect_home = _last(service, "ProtectHome")
    if protect_home is None:
        errors.append("missing ProtectHome=")
    elif protect_home.strip().lower() not in {"true", "yes", "full", "1", "on"}:
        errors.append(f"ProtectHome= must hide /home, got {protect_home!r}")

    families: list[str] = []
    for value in service.get("RestrictAddressFamilies", []):
        families.extend(value.split())
    if not families:
        errors.append("missing RestrictAddressFamilies=")
    else:
        allowed = {"AF_UNIX", "AF_INET", "AF_INET6"}
        extra = [f for f in families if f not in allowed]
        if extra:
            errors.append(f"RestrictAddressFamilies= includes unexpected {extra}")
        if "AF_INET" not in families or "AF_UNIX" not in families:
            errors.append("RestrictAddressFamilies= must include AF_UNIX and AF_INET")

    filters = service.get("SystemCallFilter", [])
    if not any("@system-service" in value.split() for value in filters):
        errors.append("missing SystemCallFilter=@system-service")

    for directive in ("CapabilityBoundingSet", "AmbientCapabilities"):
        values = service.get(directive)
        if values is not None and any(value.strip() for value in values):
            errors.append(f"{directive}= must be empty")

    write_paths: list[str] = []
    for value in service.get("ReadWritePaths", []):
        write_paths.extend(value.split())
    if not write_paths:
        errors.append("missing ReadWritePaths= for the data directory")
    else:
        for path in write_paths:
            clean = path.lstrip("-")
            if not _inside(clean, ALLOWED_WRITE_PATHS):
                errors.append(f"ReadWritePaths= exposes a path outside the data dirs: {path}")
        if not any(path.lstrip("-") == "/var/lib/spot" for path in write_paths):
            errors.append("ReadWritePaths= must include /var/lib/spot")

    if "ReadWriteDirectories" in service:
        errors.append("ReadWriteDirectories= is deprecated; use ReadWritePaths=")

    memory_wx = _last(service, "MemoryDenyWriteExecute")
    if memory_wx is not None and _truthy(memory_wx):
        errors.append("MemoryDenyWriteExecute=true breaks the WASM/JavaScript JIT engines")

    for directive in REQUIRED_DIRECTORIES:
        value = _last(service, directive)
        if value is None or "spot" not in value.split():
            errors.append(f"missing {directive}=spot")

    restart = _last(service, "Restart")
    if restart not in {"on-failure", "always"}:
        errors.append("Restart= must be on-failure or always")
    if not _last(service, "RestartSec"):
        errors.append("missing RestartSec=")

    burst = _last(unit, "StartLimitBurst")
    interval = _last(unit, "StartLimitIntervalSec")
    if not burst or not interval:
        errors.append("missing StartLimitBurst=/StartLimitIntervalSec= (start rate limit)")

    timeout = _last(service, "TimeoutStopSec")
    if not timeout:
        errors.append("missing TimeoutStopSec=")
    else:
        seconds = _duration(timeout)
        if seconds is None or seconds < 30:
            errors.append(f"TimeoutStopSec= must be at least the 30 s shutdown budget, got {timeout!r}")

    return errors


def _run(command: list[str], timeout: int = 120) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, capture_output=True, text=True, timeout=timeout)


def systemd_available() -> bool:
    return shutil.which("systemd-analyze") is not None


def _verify_copy(path: Path) -> Path:
    """Copy the unit, pointing ExecStart at a real executable for verify only."""
    text = path.read_text(encoding="utf-8")
    lines: list[str] = []
    for line in text.splitlines():
        if line.strip().startswith("ExecStart="):
            prefix, _, rest = line.partition("=")
            parts = rest.strip().split()
            if parts and parts[0].startswith("/"):
                parts[0] = "/bin/true"
                line = prefix + "=" + " ".join(parts)
        lines.append(line)
    handle, name = tempfile.mkstemp(prefix="spot-verify-", suffix=".service")
    os.close(handle)
    copy = Path(name)
    copy.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return copy


def verify_unit(path: Path) -> dict:
    tool = shutil.which("systemd-analyze")
    if tool is None:
        return {"available": False, "ok": True, "output": "systemd-analyze not found"}
    copy = _verify_copy(path)
    try:
        verify = _run([tool, "verify", str(copy)])
    finally:
        copy.unlink(missing_ok=True)
    security = _run([tool, "security", "--offline=yes", str(path)])
    output = (verify.stdout + verify.stderr).strip()
    match = re.search(r"Overall exposure level[^:]*:\s*([0-9.]+)", security.stdout + security.stderr)
    score = float(match.group(1)) if match else None
    ok = verify.returncode == 0 and score is not None and score <= SECURITY_THRESHOLD
    return {
        "available": True,
        "ok": ok,
        "verify_rc": verify.returncode,
        "output": output,
        "security_score": score,
        "threshold": SECURITY_THRESHOLD,
    }


def check_unit(path: Path, threshold: float = SECURITY_THRESHOLD,
               use_systemd_analyze: bool = True) -> dict:
    global SECURITY_THRESHOLD
    SECURITY_THRESHOLD = threshold
    if not path.is_file():
        raise CheckError(f"unit not found: {path}")
    errors = lint_unit(path.read_text(encoding="utf-8"))
    result: dict = {"check": "unit", "path": str(path), "errors": errors}
    if errors:
        result["ok"] = False
        return result
    if use_systemd_analyze and systemd_available():
        extra = verify_unit(path)
        result.update(extra)
        if not extra["ok"]:
            result["ok"] = False
            return result
    result["ok"] = True
    return result


def check_tarball(path: Path) -> dict:
    if not path.is_file():
        raise CheckError(f"tarball not found: {path}")
    expected = {
        "bin", "bin/spot", "etc", "etc/spot.toml", "etc/environment",
        "systemd", "systemd/spot.service", "systemd/spot.sysusers",
        "systemd/spot.tmpfiles", "LICENSE", "NOTICE", "README.install",
    }
    errors: list[str] = []
    with tarfile.open(path, "r:gz") as archive:
        names = archive.getnames()
        if names != sorted(names):
            errors.append("archive entries are not sorted")
        missing = expected - set(names)
        extra = set(names) - expected
        if missing:
            errors.append(f"missing entries: {sorted(missing)}")
        if extra:
            errors.append(f"unexpected entries: {sorted(extra)}")
        for member in archive.getmembers():
            if member.name.startswith("/") or ".." in Path(member.name).parts:
                errors.append(f"unsafe path in archive: {member.name}")
            if member.uid or member.gid or member.uname or member.gname:
                errors.append(f"owner not normalised on {member.name}")
            if member.mtime != int(member.mtime) or member.mtime < 0:
                errors.append(f"invalid timestamp on {member.name}")
            expected_mode = 0o755 if member.isdir() else (0o755 if member.name == "bin/spot" else 0o640 if member.name.startswith("etc/") else 0o644)
            if member.mode & 0o777 != expected_mode:
                errors.append(f"unexpected mode {member.mode & 0o777:o} on {member.name}")
        spot = archive.getmember("bin/spot") if "bin/spot" in names else None
        if spot is not None and spot.size == 0:
            errors.append("bin/spot is empty")
    return {"check": "tarball", "path": str(path), "ok": not errors, "errors": errors}


def _expected_rpm(version: str | None) -> tuple[str, str] | None:
    if not version:
        return None
    base = version.lstrip("v")
    if "-" not in base:
        return base, None
    upstream, _, pre = base.partition("-")
    return upstream, "0." + pre.replace("-", ".")


def check_rpm(path: Path, version: str | None = None) -> dict:
    tool = shutil.which("rpm")
    if tool is None:
        return {"check": "rpm", "path": str(path), "ok": True, "skipped": "rpm not found"}
    if not path.is_file():
        raise CheckError(f"rpm not found: {path}")
    errors: list[str] = []
    meta = _run([tool, "-qp", "--queryformat", "%{NAME}\\t%{VERSION}\\t%{RELEASE}\\t%{ARCH}", str(path)])
    if meta.returncode != 0:
        raise CheckError(meta.stderr.strip() or "rpm query failed")
    name, rpm_version, release, _arch = meta.stdout.strip().split("\t")
    if name != "spot":
        errors.append(f"package name is {name!r}, expected spot")
    if version:
        expected_version, expected_release = _expected_rpm(version)
        if rpm_version != expected_version:
            errors.append(f"version {rpm_version!r} != {expected_version!r}")
        if expected_release and not release.startswith(expected_release):
            errors.append(f"release {release!r} does not start with {expected_release!r}")
    files = _run([tool, "-qpl", str(path)])
    if files.returncode != 0:
        raise CheckError(files.stderr.strip() or "rpm file list failed")
    listed = [line for line in files.stdout.splitlines() if line]
    configs = _run([tool, "-qpc", str(path)])
    conffiles = set(configs.stdout.splitlines()) if configs.returncode == 0 else set()
    for required in ("/usr/bin/spot", "/usr/lib/systemd/system/spot.service",
                     "/usr/lib/sysusers.d/spot.conf", "/usr/lib/tmpfiles.d/spot.conf",
                     "/etc/spot/spot.toml", "/etc/spot/environment",
                     "/usr/share/licenses/spot/LICENSE", "/usr/share/licenses/spot/NOTICE"):
        if required not in listed:
            errors.append(f"missing {required}")
    for config in ("/etc/spot/spot.toml", "/etc/spot/environment"):
        if config not in conffiles:
            errors.append(f"{config} is not flagged as a config file")
    for item in listed:
        if item.startswith("/") and not _inside(item, ALLOWED_PACKAGE_PREFIXES):
            errors.append(f"file outside allowed prefixes: {item}")
    modes = _run([tool, "-qp", "--queryformat", "[%{FILEMODES:octal}\\t%{FILENAMES}\\n]", str(path)])
    for line in modes.stdout.splitlines():
        mode, _, item = line.partition("\t")
        if item == "/usr/bin/spot" and int(mode, 8) & 0o777 != 0o755:
            errors.append("binary is not mode 0755")
        if item == "/usr/lib/systemd/system/spot.service" and int(mode, 8) & 0o777 != 0o644:
            errors.append("unit is not mode 0644")
    return {"check": "rpm", "path": str(path), "ok": not errors, "errors": errors}


def _deb_field(tool: str, path: Path, field: str) -> str:
    run = _run([tool, "--field", str(path), field])
    if run.returncode != 0:
        raise CheckError(run.stderr.strip() or f"dpkg-deb --field {field} failed")
    return run.stdout.strip()


def check_deb(path: Path, version: str | None = None) -> dict:
    tool = shutil.which("dpkg-deb")
    if tool is None:
        return {"check": "deb", "path": str(path), "ok": True, "skipped": "dpkg-deb not found"}
    if not path.is_file():
        raise CheckError(f"deb not found: {path}")
    errors: list[str] = []
    package = _deb_field(tool, path, "Package")
    debver = _deb_field(tool, path, "Version")
    arch = _deb_field(tool, path, "Architecture")
    if package != "spot":
        errors.append(f"package is {package!r}, expected spot")
    if version:
        expected = version.lstrip("v").replace("-", "~", 1)
        if debver != expected:
            errors.append(f"version {debver!r} != {expected!r}")
    if arch not in {"amd64", "arm64", "all"}:
        errors.append(f"unexpected architecture {arch!r}")
    raw = subprocess.run([tool, "--fsys-tarfile", str(path)], capture_output=True)
    if raw.returncode != 0:
        raise CheckError(raw.stderr.decode(errors="replace").strip() or "dpkg-deb --fsys-tarfile failed")
    listed: dict[str, int] = {}
    with tarfile.open(fileobj=io.BytesIO(raw.stdout), mode="r:") as archive:
        for member in archive.getmembers():
            name = member.name[2:] if member.name.startswith("./") else member.name
            if not name or name == ".":
                continue
            if member.uid or member.gid:
                errors.append(f"owner not normalised on {name}")
            if member.isfile():
                listed[name] = member.mode & 0o777
            elif member.isdir():
                listed.setdefault(name, 0o755)
    for required in ("usr/bin/spot", "usr/lib/systemd/system/spot.service",
                     "usr/lib/sysusers.d/spot.conf", "usr/lib/tmpfiles.d/spot.conf",
                     "etc/spot/spot.toml", "etc/spot/environment",
                     "usr/share/doc/spot/LICENSE", "usr/share/doc/spot/NOTICE",
                     "usr/share/doc/spot/copyright"):
        if required not in listed:
            errors.append(f"missing {required}")
    if listed.get("usr/bin/spot") not in (None, 0o755):
        errors.append("binary is not mode 0755")
    for item in listed:
        if item.startswith("/") or ".." in Path(item).parts:
            errors.append(f"unsafe path in package: {item}")
        elif not _inside("/" + item, ALLOWED_PACKAGE_PREFIXES):
            errors.append(f"file outside allowed prefixes: {item}")
    conffiles = _run([tool, "--info", str(path), "conffiles"])
    configured = set(conffiles.stdout.splitlines()) if conffiles.returncode == 0 else set()
    for config in ("/etc/spot/spot.toml", "/etc/spot/environment"):
        if config not in configured:
            errors.append(f"{config} is not listed in conffiles")
    return {"check": "deb", "path": str(path), "ok": not errors, "errors": errors}


def check_containerfile(path: Path = DEFAULT_CONTAINERFILE) -> dict:
    if not path.is_file():
        raise CheckError(f"Containerfile not found: {path}")
    text = path.read_text(encoding="utf-8")
    logical = re.sub(r"\\\s*\n\s*", " ", text)
    lines = [line.strip() for line in logical.splitlines() if line.strip() and not line.strip().startswith("#")]
    errors: list[str] = []
    froms = [line for line in lines if line.upper().startswith("FROM ")]
    if len(froms) < 2 or not any(" AS " in line.upper() for line in froms):
        errors.append("Containerfile must be multi-stage (two FROM lines, one named stage)")
    users = [line for line in lines if line.upper().startswith("USER ")]
    if not users:
        errors.append("missing USER (the container must not run as root)")
    elif users[-1].split(None, 1)[1].strip().lower() in {"root", "0", "0:0", "root:root"}:
        errors.append("USER must not be root")
    if not any(line.upper().startswith("HEALTHCHECK ") for line in lines):
        errors.append("missing HEALTHCHECK")
    if not any(line.upper().startswith("STOPSIGNAL SIGTERM") for line in lines):
        errors.append("missing STOPSIGNAL SIGTERM")
    if not any(line.upper().startswith("VOLUME ") and "/var/lib/spot" in line for line in lines):
        errors.append("missing VOLUME /var/lib/spot")
    if not any(line.upper().startswith("LABEL ") and "org.opencontainers.image.version" in line for line in lines):
        errors.append("missing org.opencontainers.image.version label")
    for directive in ("ENTRYPOINT", "CMD"):
        found = [line for line in lines if line.upper().startswith(directive + " ")]
        if not found:
            errors.append(f"missing {directive}")
        elif not found[-1].split(None, 1)[1].strip().startswith("["):
            errors.append(f"{directive} must use exec (JSON array) form")
    return {"check": "containerfile", "path": str(path), "ok": not errors, "errors": errors}


def check_container(image: str) -> dict:
    tool = shutil.which("podman")
    if tool is None:
        return {"check": "container", "image": image, "ok": True, "skipped": "podman not found"}
    errors: list[str] = []
    inspect = _run([tool, "image", "inspect", "--format", "{{json .Config}}", image])
    if inspect.returncode != 0:
        raise CheckError(inspect.stderr.strip() or f"image {image} not found")
    config = json.loads(inspect.stdout)
    user = (config.get("User") or "").strip()
    if not user or user.split(":")[0] in {"root", "0"}:
        errors.append("image runs as root")
    if not config.get("Healthcheck"):
        errors.append("image has no HEALTHCHECK")
    labels = config.get("Labels") or {}
    if "org.opencontainers.image.version" not in labels:
        errors.append("image lacks the version label")
    smoke = _run([tool, "run", "--rm", image, "--help"], timeout=120)
    if smoke.returncode != 0:
        errors.append(f"container smoke run failed with exit {smoke.returncode}")
    return {"check": "container", "image": image, "ok": not errors, "errors": errors}


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    unit = sub.add_parser("unit", help="lint and analyse the systemd unit")
    unit.add_argument("path", nargs="?", type=Path, default=DEFAULT_UNIT)
    unit.add_argument("--threshold", type=float, default=SECURITY_THRESHOLD)
    unit.add_argument("--no-systemd-analyze", action="store_true")
    tarball = sub.add_parser("tarball", help="check a release tarball")
    tarball.add_argument("path", type=Path)
    rpm = sub.add_parser("rpm", help="check an RPM")
    rpm.add_argument("path", type=Path)
    rpm.add_argument("--version")
    deb = sub.add_parser("deb", help="check a Debian package")
    deb.add_argument("path", type=Path)
    deb.add_argument("--version")
    containerfile = sub.add_parser("containerfile", help="lint the Containerfile")
    containerfile.add_argument("path", nargs="?", type=Path, default=DEFAULT_CONTAINERFILE)
    container = sub.add_parser("container", help="smoke-test a built image")
    container.add_argument("image")
    return parser.parse_args(argv)


def run_check(args: argparse.Namespace) -> dict:
    if args.command == "unit":
        return check_unit(args.path, args.threshold, not args.no_systemd_analyze)
    if args.command == "tarball":
        return check_tarball(args.path)
    if args.command == "rpm":
        return check_rpm(args.path, args.version)
    if args.command == "deb":
        return check_deb(args.path, args.version)
    if args.command == "containerfile":
        return check_containerfile(args.path)
    if args.command == "container":
        return check_container(args.image)
    raise SystemExit(f"unknown command {args.command!r}")


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        result = run_check(args)
    except CheckError as exc:
        print(f"FAIL {exc}")
        return 1
    if result.get("skipped"):
        print(f"SKIP {result['check']} {result['skipped']}")
        return 0
    if result["ok"]:
        extra = ""
        if "security_score" in result:
            extra = f" security={result['security_score']}/{result['threshold']}"
        print(f"OK {result['check']}{extra}")
        return 0
    print(json.dumps(result, indent=2, sort_keys=True))
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
