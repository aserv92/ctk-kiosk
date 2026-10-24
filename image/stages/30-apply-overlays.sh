#!/bin/sh

set_file_permissions() {
  fs_path="$1"
  permissions_file="$2"

  while read -r line; do
    [ -z "$line" ] && continue

    uid_gid=$(echo "$line" | awk '{print $1}')
    mode=$(echo "$line" | awk '{print $2}')
    path=$(echo "$line" | awk '{print $3}')

    uid=$(echo "$uid_gid" | cut -d: -f1)
    gid=$(echo "$uid_gid" | cut -d: -f2)

    if [ ! -e "${fs_path}/${path}" ]; then
      echo "Missing path: $path" >&2
      exit 1
    fi

    chown "$uid:$gid" "${fs_path}/${path}"
    chmod "$mode" "${fs_path}/${path}"
  done < "$permissions_file"
}

apk add --no-cache rsync

echo "Applying rootfs overlay..."
rsync -a /mnt/overlays/root_fs/ /

echo "Setting file permissions and ownership for root filesystem..."
set_file_permissions / /mnt/overlays/root_fs.permissions

apk del rsync
