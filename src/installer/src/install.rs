use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use crate::disks::{self, boot_disk};

pub struct Progress {
    status: Mutex<String>,
    written: AtomicU64,
    total: AtomicU64,
    generation: AtomicU64,
}

impl Progress {
    pub fn new() -> Self {
        Self {
            status: Mutex::new(String::new()),
            written: AtomicU64::new(0),
            total: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        }
    }

    pub fn status(&self) -> String {
        self.status.lock().expect("progress lock").clone()
    }

    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn touch(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn set_status(&self, status: impl Into<String>) {
        *self.status.lock().expect("progress lock") = status.into();
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn set_written(&self, written: u64) {
        self.written.store(written, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn install_disk(disk: &str, progress: &Progress) -> Result<(), String> {
    let payload = std::env::var("CTK_PAYLOAD").unwrap_or_else(|_| "/opt/ctk/kiosk.img.xz".to_string());
    let size_file = std::env::var("CTK_PAYLOAD_SIZE")
        .unwrap_or_else(|_| "/opt/ctk/kiosk.img.size".to_string());

    if !disks::is_block_device(disk) {
        return Err(format!("{disk} is not a block device."));
    }
    if boot_disk().as_deref() == Some(disk) {
        return Err(format!(
            "Refusing to erase {disk}, which is the drive this installer booted from."
        ));
    }
    if !Path::new(&payload).is_file() || !Path::new(&size_file).is_file() {
        return Err("The kiosk image payload is missing from this USB drive.".to_string());
    }

    let image_bytes = read_u64(&size_file)?;
    let disk_bytes = device_bytes(disk)?;
    if disk_bytes < image_bytes {
        return Err(format!(
            "{disk} is {}. The kiosk image needs {}.",
            disks::human_size(disk_bytes),
            disks::human_size(image_bytes)
        ));
    }

    progress.set_total(image_bytes);
    progress.set_written(0);
    progress.set_status("Checking the kiosk image...");
    run("xz", &["-t", &payload])?;

    progress.set_status("Writing the kiosk image...");
    write_payload(&payload, disk, image_bytes, progress)?;
    let _ = Command::new("sync").status();

    try_run("blockdev", &["--rereadpt", disk]);
    settle(disk);

    if disk_bytes > image_bytes {
        // The image is 2G, so its backup GPT header is not at the end of the disk.
        progress.set_status("Expanding the system partition...");
        run("sfdisk", &["--relocate", "gpt-bak-std", disk])?;
        let dump = command_output("sfdisk", &["-d", disk])?;
        let (partnum, node) = system_partition(&dump, disk).ok_or_else(|| {
            format!("The system partition was not found on {disk}.\n{dump}")
        })?;
        if !Path::new(&node).exists() {
            return Err(format!("The system partition node {node} did not appear."));
        }

        let before = device_bytes(&node).unwrap_or(0);
        feed("sfdisk", &["-N", &partnum.to_string(), disk], b", +\n")?;
        let _ = Command::new("sync").status();
        settle(disk);

        let expected = disks::partition_path(disk, partnum);
        if !wait_for_node(&expected) {
            return Err(format!("The expanded partition {expected} did not appear."));
        }
        let after = device_bytes(&expected).unwrap_or(0);
        if after <= before {
            return Err(format!(
                "The kernel kept the old size of {expected}. The root filesystem was not expanded."
            ));
        }

        progress.set_status("Expanding the root filesystem...");
        e2fsck(&expected)?;
        run("resize2fs", &[&expected])?;
    }

    let _ = Command::new("sync").status();
    progress.set_written(image_bytes);
    progress.set_status("Installed.");
    Ok(())
}

pub fn system_partition(dump: &str, disk: &str) -> Option<(u32, String)> {
    for line in dump.lines() {
        if !line.contains("name=\"system\"") {
            continue;
        }
        let node = line.split(':').next()?.trim();
        let suffix = node.strip_prefix(disk)?;
        let suffix = suffix.trim_start_matches('p');
        let partnum = suffix.parse().ok()?;
        return Some((partnum, node.to_string()));
    }
    None
}

fn write_payload(payload: &str, disk: &str, image_bytes: u64, progress: &Progress) -> Result<(), String> {
    let mut child = Command::new("xz")
        .args(["-dc", payload])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("xz: {err}"))?;
    let mut decoded = child.stdout.take().expect("xz stdout");
    let mut file = OpenOptions::new()
        .write(true)
        .open(disk)
        .map_err(|err| format!("Failed to open {disk}: {err}"))?;

    let mut buffer = vec![0u8; 1024 * 1024];
    let mut written = 0u64;
    let mut since_report = 0u64;
    loop {
        let count = decoded
            .read(&mut buffer)
            .map_err(|err| format!("Reading the kiosk image failed: {err}"))?;
        if count == 0 {
            break;
        }
        file.write_all(&buffer[..count])
            .map_err(|err| format!("Writing {disk} failed: {err}"))?;
        written += count as u64;
        since_report += count as u64;
        if since_report >= 8 * 1024 * 1024 {
            progress.set_written(written);
            since_report = 0;
        }
    }
    progress.set_written(written);
    file.sync_all()
        .map_err(|err| format!("Flushing {disk} failed: {err}"))?;
    drop(file);

    let output = child
        .wait_with_output()
        .map_err(|err| format!("xz: {err}"))?;
    if !output.status.success() {
        return Err(command_failure("xz", &output.stderr, &output.stdout));
    }
    if written != image_bytes {
        return Err(format!(
            "Wrote {} of the kiosk image, and the image is {}.",
            disks::human_size(written),
            disks::human_size(image_bytes)
        ));
    }
    Ok(())
}

fn e2fsck(node: &str) -> Result<(), String> {
    let output = Command::new("e2fsck")
        .args(["-f", "-p", node])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("e2fsck: {err}"))?;
    let code = output.status.code().unwrap_or(8);
    if code > 1 {
        return Err(command_failure("e2fsck", &output.stderr, &output.stdout));
    }
    Ok(())
}

fn settle(disk: &str) {
    try_run("partx", &["-u", disk]);
    try_run("blockdev", &["--rereadpt", disk]);
    try_run("mdev", &["-s"]);
    try_run("udevadm", &["settle", "--timeout=5"]);
}

fn wait_for_node(path: &str) -> bool {
    for _ in 0..25 {
        if Path::new(path).exists() {
            return true;
        }
        try_run("mdev", &["-s"]);
        try_run("udevadm", &["settle", "--timeout=1"]);
        thread::sleep(Duration::from_millis(200));
    }
    Path::new(path).exists()
}

fn device_bytes(path: &str) -> Result<u64, String> {
    let name = path.trim_start_matches("/dev/");
    let text = fs::read_to_string(format!("/sys/class/block/{name}/size"))
        .or_else(|_| fs::read_to_string(format!("/sys/block/{name}/size")))
        .map_err(|_| format!("Cannot read the size of {path}."))?;
    let sectors = text
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("The size of {path} is not a number."))?;
    Ok(sectors.saturating_mul(512))
}

fn read_u64(path: &str) -> Result<u64, String> {
    let text = fs::read_to_string(path).map_err(|err| format!("Cannot read {path}: {err}"))?;
    text.trim()
        .parse::<u64>()
        .map_err(|_| format!("{path} does not contain the image size."))
}

fn run(command: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("{command}: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_failure(command, &output.stderr, &output.stdout))
    }
}

fn feed(command: &str, args: &[&str], stdin: &[u8]) -> Result<(), String> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("{command}: {err}"))?;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin)
        .map_err(|err| format!("{command}: {err}"))?;
    let output = child
        .wait_with_output()
        .map_err(|err| format!("{command}: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_failure(command, &output.stderr, &output.stdout))
    }
}

fn command_output(command: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("{command}: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(command_failure(command, &output.stderr, &output.stdout))
    }
}

fn try_run(command: &str, args: &[&str]) {
    let _ = Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn command_failure(command: &str, stderr: &[u8], stdout: &[u8]) -> String {
    let mut detail = String::from_utf8_lossy(stderr).trim().to_string();
    if detail.is_empty() {
        detail = String::from_utf8_lossy(stdout).trim().to_string();
    }
    if detail.chars().count() > 240 {
        detail = detail.chars().take(240).collect();
    }
    if detail.is_empty() {
        format!("{command} failed.")
    } else {
        format!("{command} failed: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_named_system_partition() {
        let dump = "\
label: gpt
device: /dev/sda
/dev/sda1 : start=2048, size=262144, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name=\"efi\"
/dev/sda2 : start=264192, size=3930112, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name=\"system\"
";
        assert_eq!(
            system_partition(dump, "/dev/sda"),
            Some((2, "/dev/sda2".to_string()))
        );
    }

    #[test]
    fn finds_a_numbered_disk_partition() {
        let dump = "/dev/nvme0n1p2 : start=1, size=2, name=\"system\"\n";
        assert_eq!(
            system_partition(dump, "/dev/nvme0n1"),
            Some((2, "/dev/nvme0n1p2".to_string()))
        );
    }
}
