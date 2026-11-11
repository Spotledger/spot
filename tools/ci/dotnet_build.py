#!/usr/bin/env python3
"""Run `dotnet build` for CI on a host shared by many runners (the CI runner notes).

Ten runner instances share one workstation. Several .NET builds at once each start a dozen
MSBuild worker nodes, memory runs short, and the kernel kills the nodes together; the build then
fails with `MSB4166: Child node "N" exited prematurely` although nothing is wrong with the code.
This wrapper does three things, and nothing else:

1. takes a host-wide lock so only one build of the .NET client runs at a time on a host
   (`SPOT_DOTNET_LOCK`, default `<tmp>/spot-dotnet-build.lock`; skipped where `fcntl` is
   missing, e.g. Windows);
2. caps MSBuild parallelism (`-maxcpucount:2`), turns off node reuse and the shared compiler
   server so no worker outlives the build;
3. retries the build ONCE, and only when every error line in the output is MSB4166. Any real
   compiler error, warning-as-error or other failure is never retried.

Usage: dotnet_build.py <solution> [extra dotnet build args]
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tempfile
import time

ERROR_CODE = re.compile(r"\berror\s+([A-Za-z]+\d+)\s*:")
INFRA_CODE = "MSB4166"
BASE_ARGS = ["-c", "Release", "-warnaserror", "-maxcpucount:2", "-nodeReuse:false", "-p:UseSharedCompilation=false"]


def only_infra_failure(output: str) -> bool:
    """True when the output has MSB4166 errors and no other error code at all."""
    codes = set(ERROR_CODE.findall(output))
    return INFRA_CODE in codes and codes == {INFRA_CODE}


def build_env(env: dict[str, str] | None = None) -> dict[str, str]:
    out = dict(os.environ if env is None else env)
    out.setdefault("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
    out.setdefault("DOTNET_NOLOGO", "1")
    out["MSBUILDDISABLENODEREUSE"] = "1"
    return out


def run_once(cmd: list[str], env: dict[str, str]) -> tuple[int, str]:
    proc = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace")
    lines: list[str] = []
    assert proc.stdout is not None
    for line in proc.stdout:
        sys.stdout.write(line)
        lines.append(line)
    return proc.wait(), "".join(lines)


def shutdown_servers(env: dict[str, str]) -> None:
    subprocess.run(["dotnet", "build-server", "shutdown"], env=env, check=False, capture_output=True)


def build(args: list[str], runner=run_once) -> int:
    cmd = ["dotnet", "build", *args, *BASE_ARGS]
    env = build_env()
    rc, out = runner(cmd, env)
    if rc != 0 and only_infra_failure(out):
        print(f"::warning::dotnet build lost its MSBuild worker nodes ({INFRA_CODE}); retrying once", flush=True)
        shutdown_servers(env)
        time.sleep(5)
        rc, _ = runner(cmd, env)
    shutdown_servers(env)
    return rc


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    try:
        import fcntl
    except ImportError:
        return build(argv)
    path = os.environ.get("SPOT_DOTNET_LOCK", os.path.join(tempfile.gettempdir(), "spot-dotnet-build.lock"))
    with open(path, "a+") as lock:
        waited = time.monotonic()
        fcntl.flock(lock, fcntl.LOCK_EX)
        if time.monotonic() - waited > 1:
            print(f"waited {time.monotonic() - waited:.0f}s for another .NET build on this host", flush=True)
        return build(argv)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
