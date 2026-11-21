# The Ubuntu 24.04 LTS conformance image (WP-ADM-04.T3).
#
# Ubuntu LTS is the recommended production target on the Debian side; this image adds the
# tools the storage, time and network tests drive, which the base image does not carry. The
# tests run inside a `--systemd=always` container started from it.
FROM docker.io/library/ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get -y --no-install-recommends install \
        systemd \
        btrfs-progs \
        lvm2 \
        chrony \
        util-linux \
        iproute2 \
        procps \
        e2fsprogs \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

CMD ["/sbin/init"]
