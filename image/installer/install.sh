#!/bin/sh
# Start X and the graphical installer. The program returns 0 after the
# kiosk image is written and the user asks to reboot.
set -eu

export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

modprobe i915 2>/dev/null || true
modprobe hid-multitouch 2>/dev/null || true
modprobe atmel_mxt_ts 2>/dev/null || true

if [ ! -x /opt/ctk/ctk-installer ]; then
  echo "The installer program is missing." >&2
  exit 1
fi

# xinit treats a server name as extra arguments unless it starts with / or .
server=$(command -v Xorg || true)
if [ -z "$server" ]; then
  echo "Xorg is not installed." >&2
  exit 1
fi
exec xinit /opt/ctk/ctk-installer -- "$server" :0 vt1 -keeptty -nolisten tcp
