# Packaging test images (WP-PRD-09.T2)

One directory per distribution, each with a `Containerfile` that builds a **booted**
distribution: systemd is PID 1 (`podman run --systemd=always`), the package manager works,
and the few tools the scenarios drive are installed (curl for the health probe, util-linux
and procps for `stat`, `install` and `kill`).

These are test images, not the product image. The runtime image customers run is
`packaging/linux/container/Containerfile`; the packages themselves are built by
`tools/packaging/build.py`.

| Directory | Base | Package tooling |
| --- | --- | --- |
| `fedora-44/` | `registry.fedoraproject.org/fedora:44` | rpm, dnf |
| `almalinux-10/` | `quay.io/almalinuxorg/almalinux:10` | rpm, dnf (RHEL-compatible) |
| `ubuntu-2404/` | `docker.io/library/ubuntu:24.04` | dpkg, apt |

The bases match `harness/agent-linux/docker/*`, so a machine that already has those images
pulls nothing new for the distribution layer.

`tools/packaging/verify/run.py` builds each image and runs the scenarios inside it; you do
not normally build these by hand. The table above is the only list of images: adding a
distribution means adding a directory here and one `Distro` entry in
`tools/packaging/verify/distros.py`, and the driver picks both up.
