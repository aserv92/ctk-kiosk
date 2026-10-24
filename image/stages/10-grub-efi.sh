#!/bin/sh

apk add --no-cache grub-efi

boot_line=$(cat /boot/startup.nsh)
kernel=${boot_line%% *}
rest=${boot_line#* }
initrd=$(printf '%s\n' "$rest" | sed -n 's/.*initrd=\([^ ]*\).*/\1/p')
cmdline=$(printf '%s\n' "$rest" | sed "s/initrd=${initrd}//")

mkdir -p /boot/EFI/BOOT
cat > /tmp/grub.cfg <<GRUB
set timeout=0
search --file --set=root /${kernel}
linux /${kernel} ${cmdline}
initrd /${initrd}
boot
GRUB

grub-mkimage -O x86_64-efi -o /boot/EFI/BOOT/BOOTX64.EFI -p /EFI/BOOT \
  part_gpt fat search search_fs_file normal linux configfile \
  -c /tmp/grub.cfg

rm -f /tmp/grub.cfg
apk del grub-efi
