# The AlmaLinux 10 conformance image (WP-ADM-04.T3).
#
# AlmaLinux is the RHEL-compatible production recommendation; this image adds the tools
# the storage, time and network tests drive, which the base image does not carry. The tests
# run inside a `--systemd=always` container started from it.
FROM quay.io/almalinuxorg/almalinux:10

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

CMD ["/sbin/init"]
