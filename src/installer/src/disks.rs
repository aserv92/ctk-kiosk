use std::fs;
use std::os::unix::fs::FileTypeExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskKind {
    Internal,
    Removable,
    Usb,
}

impl DiskKind {
    pub fn label(self) -> &'static str {
        match self {
            DiskKind::Internal => "Internal drive",
            DiskKind::Removable => "Removable drive",
            DiskKind::Usb => "USB drive",
        }
    }

    fn rank(self) -> u8 {
        match self {
            DiskKind::Internal => 0,
            DiskKind::Removable => 1,
            DiskKind::Usb => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub path: String,
    pub size: u64,
    pub model: String,
    pub kind: DiskKind,
}

pub fn human_size(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

pub fn list_disks() -> Vec<Disk> {
    let boot = boot_disk();
    let mut disks = Vec::new();
    let entries = match fs::read_dir("/sys/block") {
        Ok(entries) => entries,
        Err(_) => return disks,
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if skip_disk(&name) {
            continue;
        }
        if !entry.path().join("device").exists() {
            continue;
        }
        let path = format!("/dev/{name}");
        if boot.as_deref() == Some(path.as_str()) {
            continue;
        }
        disks.push(Disk {
            size: disk_bytes(&name),
            model: disk_model(&name),
            kind: disk_kind(&name),
            path,
        });
    }

    disks.sort_by(|a, b| {
        a.kind
            .rank()
            .cmp(&b.kind.rank())
            .then_with(|| a.path.cmp(&b.path))
    });
    disks
}

pub fn boot_disk() -> Option<String> {
    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    boot_disk_from_mounts(&mounts)
}

pub fn boot_disk_from_mounts(mounts: &str) -> Option<String> {
    for line in mounts.lines() {
        let mut parts = line.split_whitespace();
        let (Some(source), Some(target)) = (parts.next(), parts.next()) else {
            continue;
        };
        if target == "/" && source.starts_with("/dev/") {
            return Some(parent_disk(source));
        }
    }
    None
}

pub fn parent_disk(device: &str) -> String {
    let name = device.rsplit('/').next().unwrap_or(device);
    let disk_name = if let Some((head, part)) = name.rsplit_once('p') {
        let numbered = name.starts_with("nvme")
            || name.starts_with("mmcblk")
            || name.starts_with("loop");
        if numbered && !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) {
            head
        } else {
            trim_partition(name)
        }
    } else {
        trim_partition(name)
    };
    if device.starts_with("/dev/") {
        format!("/dev/{disk_name}")
    } else {
        disk_name.to_string()
    }
}

pub fn partition_path(disk: &str, partnum: u32) -> String {
    let name = disk.rsplit('/').next().unwrap_or(disk);
    if name
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_digit())
    {
        format!("{disk}p{partnum}")
    } else {
        format!("{disk}{partnum}")
    }
}

pub fn is_block_device(path: &str) -> bool {
    fs::metadata(path)
        .map(|meta| meta.file_type().is_block_device())
        .unwrap_or(false)
}

fn trim_partition(name: &str) -> &str {
    name.trim_end_matches(|c: char| c.is_ascii_digit())
}

fn skip_disk(name: &str) -> bool {
    name.starts_with("loop")
        || name.starts_with("ram")
        || name.starts_with("fd")
        || name.starts_with("nbd")
        || name.starts_with("zram")
        || name.starts_with("dm-")
        || name.starts_with("sr")
}

fn disk_bytes(name: &str) -> u64 {
    fs::read_to_string(format!("/sys/block/{name}/size"))
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_mul(512)
}

fn disk_model(name: &str) -> String {
    let text = fs::read_to_string(format!("/sys/block/{name}/device/model")).unwrap_or_default();
    let model = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if model.is_empty() {
        "unknown model".to_string()
    } else {
        model
    }
}

fn disk_kind(name: &str) -> DiskKind {
    if name.starts_with("mmcblk") {
        return DiskKind::Removable;
    }
    if is_usb(name) {
        return DiskKind::Usb;
    }
    if fs::read_to_string(format!("/sys/block/{name}/removable"))
        .ok()
        .is_some_and(|text| text.trim() == "1")
    {
        return DiskKind::Removable;
    }
    DiskKind::Internal
}

fn is_usb(name: &str) -> bool {
    let Ok(start) = fs::canonicalize(format!("/sys/block/{name}")) else {
        return false;
    };
    let mut path = Some(start);
    while let Some(dir) = path {
        if dir.join("idVendor").is_file() {
            return true;
        }
        path = dir.parent().map(std::path::Path::to_path_buf);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(2 * 1024 * 1024), "2 MiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3 GiB");
    }

    #[test]
    fn parent_of_a_partition_is_the_disk() {
        assert_eq!(parent_disk("/dev/sda2"), "/dev/sda");
        assert_eq!(parent_disk("/dev/vda1"), "/dev/vda");
        assert_eq!(parent_disk("/dev/nvme0n1p2"), "/dev/nvme0n1");
        assert_eq!(parent_disk("/dev/mmcblk0p1"), "/dev/mmcblk0");
        assert_eq!(parent_disk("/dev/loop0p1"), "/dev/loop0");
        assert_eq!(parent_disk("/dev/sda"), "/dev/sda");
    }

    #[test]
    fn partition_nodes_follow_the_kernel_name() {
        assert_eq!(partition_path("/dev/sda", 2), "/dev/sda2");
        assert_eq!(partition_path("/dev/nvme0n1", 2), "/dev/nvme0n1p2");
        assert_eq!(partition_path("/dev/loop0", 1), "/dev/loop0p1");
    }

    #[test]
    fn boot_disk_comes_from_the_root_mount() {
        let mounts = "\
overlay / overlay rw 0 0
/dev/sdb2 / ext4 rw 0 0
";
        assert_eq!(
            boot_disk_from_mounts(mounts).as_deref(),
            Some("/dev/sdb")
        );

        let overlay_only = "overlay / overlay rw 0 0\n";
        assert_eq!(boot_disk_from_mounts(overlay_only), None);
    }
}
