#!/bin/sh
# Install X and boot kiosk-launcher on tty1. Runs inside the image chroot.
set -eu

if [ ! -x /mnt/kiosk/kiosk-launcher ]; then
  echo "kiosk-launcher binary is missing. Run ./bin/build-kiosk." >&2
  exit 1
fi

apk add --no-cache \
  alsa-utils \
  chromium \
  eudev \
  font-cursor-misc \
  iw \
  linux-firmware-i915 \
  linux-firmware-mrvl \
  wpa_supplicant \
  xinit \
  xorg-server \
  xf86-input-libinput

# X uses udev to see the Type Cover, mouse, and touchscreen.
rm -f /etc/runlevels/sysinit/mdev
install -d /etc/runlevels/sysinit /etc/runlevels/default
for svc in udev udev-trigger udev-settle; do
  if [ ! -x "/etc/init.d/$svc" ]; then
    echo "OpenRC service $svc is missing from eudev." >&2
    exit 1
  fi
  ln -sf "/etc/init.d/$svc" "/etc/runlevels/sysinit/$svc"
done
if [ -x /etc/init.d/udev-postmount ]; then
  ln -sf /etc/init.d/udev-postmount /etc/runlevels/default/udev-postmount
fi

install -d -m 0755 /etc/X11/xorg.conf.d
cat > /etc/X11/Xwrapper.config <<'EOF'
allowed_users=anybody
needs_root_rights=yes
EOF
cat > /etc/X11/xorg.conf.d/20-modesetting.conf <<'EOF'
Section "Device"
    Identifier "Card0"
    Driver "modesetting"
    Option "AccelMethod" "none"
EndSection
EOF

install -d -m 0755 /opt/ctk
install -m 0755 /mnt/kiosk/kiosk-launcher /opt/ctk/kiosk-launcher
install -m 0755 /mnt/kiosk/kiosk-session /opt/ctk/kiosk-session
install -m 0755 /mnt/kiosk/kiosk-tty /opt/ctk/kiosk-tty

if ! grep -q '^tty1::respawn:' /etc/inittab; then
  echo "tty1 getty line is missing from /etc/inittab" >&2
  exit 1
fi
sed -i 's|^tty1::respawn:.*|tty1::respawn:/opt/ctk/kiosk-tty|' /etc/inittab
