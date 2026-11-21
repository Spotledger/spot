# The Fedora 44 conformance image (WP-ADM-04.T3).
#
# The base Fedora image has systemd but not the tools the storage, time and network tests
# drive, so the harness builds this image rather than pulling the base. The tests run
# inside a `--systemd=always` container started from it: systemd is PID 1, and each test is
# run with `podman exec`.
FROM registry.fedoraproject.org/fedora:44

RUN dnf -y install \
        systemd \
        btrfs-progs \
        lvm2 \
        chrony \
        util-linux \
        iproute \
        procps-ng \
        e2fsprogs \
    && dnf clean all \
    && rm -rf /var/cache/dnf

# systemd is the entry point; the harness starts the container detached.
CMD ["/sbin/init"]
