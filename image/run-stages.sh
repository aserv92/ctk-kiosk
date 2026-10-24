#!/bin/sh
# Runs inside the image chroot. This directory is bind-mounted at /mnt.
set -eu

cd /

for script in /mnt/stages/*.sh; do
  [ -f "$script" ] || continue
  echo "${script##*/}"
  # shellcheck disable=SC1090
  . "$script"
done
