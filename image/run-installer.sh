#!/bin/sh
# Runs inside the installer image chroot. This directory is bind-mounted at /mnt.
set -eu

cd /

echo "10-grub-efi.sh"
# shellcheck disable=SC1091
. /mnt/stages/10-grub-efi.sh

echo "installer/setup.sh"
# shellcheck disable=SC1091
. /mnt/installer/setup.sh
