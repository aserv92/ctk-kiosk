#!/usr/bin/env bash

set -euo pipefail

blk.assert.is.block.device() {
  local dev="$1"

  if [[ ! -b "$dev" ]]; then
    echo "Expected: '$dev' to be a block device" >&2
    echo "Actual: '$dev' is not a block device" >&2
    return 1
  fi
}

blk.unmount() {
  local device="$1"
  local mnt_dir="$2"

  for part in $(lsblk -ln -o NAME,MOUNTPOINT "$device" | awk '$2 != "" {print "/dev/" $1}'); do
    echo "Un-mounting $part"
    docker run \
      -it \
      --rm \
      --privileged \
      --device "${device}" \
      -v "${mnt_dir}:/mnt:shared" \
      alpine \
      umount "$part"
  done
}

blk.mount() {
  local device="$1"
  local mnt_dir="$2"
  local name="$3"

  echo "Mounting $device (${name})"

  mkdir -p "${mnt_dir}/${name}"

  docker run \
    -it \
    --rm \
    --privileged \
    --device "${device}" \
    -v "${mnt_dir}:/mnt:shared" \
    alpine \
    mount "$device" "mnt/${name}"
}
