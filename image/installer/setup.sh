#!/bin/sh
# Build-time setup for the USB installer. Runs inside the image chroot.
set -eu

if [ -z "${IMG_NAME:-}" ] || [ ! -f "/mnt/output/${IMG_NAME}.img" ]; then
  echo "Kiosk image /mnt/output/${IMG_NAME:-<unset>}.img is missing." >&2
  exit 1
fi

if [ ! -x /mnt/installer/ctk-installer ]; then
  echo "ctk-installer binary is missing. Run ./bin/build-installer." >&2
  exit 1
fi

printf '%s\n' ctk-installer > /etc/hostname

apk add --no-cache \
  e2fsprogs \
  e2fsprogs-extra \
  eudev \
  font-cursor-misc \
  linux-firmware-i915 \
  partx \
  sfdisk \
  xinit \
  xorg-server \
  xf86-input-libinput \
  xz

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
install -m 0755 /mnt/installer/ctk-installer /opt/ctk/ctk-installer
install -m 0755 /mnt/installer/install.sh /opt/ctk/install.sh
install -m 0755 /mnt/installer/install-tty /opt/ctk/install-tty

stat -c %s "/mnt/output/${IMG_NAME}.img" > /opt/ctk/kiosk.img.size
echo "Compressing ${IMG_NAME}.img into the installer..."
xz -2 -T0 -c "/mnt/output/${IMG_NAME}.img" > /opt/ctk/kiosk.img.xz

if ! grep -q '^tty1::respawn:' /etc/inittab; then
  echo "tty1 getty line is missing from /etc/inittab" >&2
  exit 1
fi
sed -i 's|^tty1::respawn:.*|tty1::respawn:/opt/ctk/install-tty|' /etc/inittab

cat > /etc/motd <<'EOF'
ctk-kiosk installer.
The internal disk has not been changed.
Run /opt/ctk/install.sh to open the installer again.
EOF
