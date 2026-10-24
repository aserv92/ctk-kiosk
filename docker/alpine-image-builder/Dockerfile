# Wraps alpine-make-vm-image so disk images can be built without an Alpine host.
# Run it privileged, with the host nbd module loaded (`modprobe nbd max_part=16`),
# and mount a directory for the image file.
FROM alpine:3.23.7

# On Alpine the script installs these tools into a virtual package and deletes
# that package on exit. Preinstall them and skip that step.
ENV INSTALL_HOST_PKGS=no

RUN apk add --no-cache \
        alpine-make-vm-image \
        blkid \
        btrfs-progs \
        dosfstools \
        e2fsprogs \
        mount \
        qemu-img \
        rsync \
        sfdisk \
        umount \
        util-linux-misc \
        xfsprogs \
    && command -v alpine-make-vm-image \
    && command -v qemu-img \
    && command -v qemu-nbd \
    && command -v blockdev \
    && command -v blkid \
    && command -v sfdisk \
    && command -v mkfs.ext4 \
    && command -v mkfs.fat \
    && command -v mkfs.btrfs \
    && command -v mkfs.xfs \
    && command -v rsync \
    && command -v mdev \
    && alpine-make-vm-image --version

WORKDIR /workdir

ENTRYPOINT ["alpine-make-vm-image"]
