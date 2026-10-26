use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;

use crate::config::Config;

const WPA_CONF: &str = "/etc/wpa_supplicant/wpa_supplicant.conf";
const WPA_ARGS: &str = "/etc/conf.d/wpa_supplicant";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WifiStatus {
    pub text: String,
}

pub fn status() -> WifiStatus {
    let Some(iface) = wireless_interface() else {
        return WifiStatus {
            text: "No wireless device".to_string(),
        };
    };
    if let Some(text) = command_output("wpa_cli", &["-i", &iface, "status"]) {
        if let Some(text) = describe_wpa(&text) {
            return WifiStatus { text };
        }
    }
    if let Some(text) = command_output("iw", &["dev", &iface, "link"]) {
        return WifiStatus {
            text: describe_iw(&text),
        };
    }
    let oper = fs::read_to_string(format!("/sys/class/net/{iface}/operstate")).unwrap_or_default();
    WifiStatus {
        text: if oper.trim() == "up" {
            "Connected".to_string()
        } else {
            "Disconnected".to_string()
        },
    }
}

pub fn apply(config: &Config) -> Result<(), String> {
    let iface = wireless_interface().ok_or_else(|| "No wireless device.".to_string())?;
    write_supplicant(config)?;
    write_service_args(&iface)?;
    ensure_dhcp(&iface)?;
    let _ = command_output("ip", &["link", "set", &iface, "up"]);
    if command_exists("rc-service") {
        let _ = command_output("rc-update", &["add", "wpa_supplicant", "boot"]);
        if command_output("rc-service", &["wpa_supplicant", "restart"]).is_none() {
            command_output("rc-service", &["wpa_supplicant", "start"])
                .ok_or_else(|| "Wi-Fi service did not start.".to_string())?;
        }
        let _ = command_output("ifup", &[&iface]);
        return Ok(());
    }
    if command_output("wpa_cli", &["-i", &iface, "status"]).is_some() {
        command_output("wpa_cli", &["-i", &iface, "reconfigure"])
            .ok_or_else(|| "Wi-Fi did not accept the new network.".to_string())?;
        let _ = command_output("wpa_cli", &["-i", &iface, "reconnect"]);
        return Ok(());
    }
    command_output(
        "wpa_supplicant",
        &["-B", "-i", &iface, "-c", WPA_CONF],
    )
    .ok_or_else(|| "Wi-Fi did not start.".to_string())?;
    Ok(())
}

pub fn describe_wpa(text: &str) -> Option<String> {
    let mut state = None;
    let mut ssid = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("wpa_state=") {
            state = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("ssid=") {
            ssid = Some(value.trim());
        }
    }
    let state = state?;
    Some(match state {
        "COMPLETED" => match ssid {
            Some(ssid) if !ssid.is_empty() => format!("Connected to {ssid}"),
            _ => "Connected".to_string(),
        },
        "DISCONNECTED" | "INACTIVE" | "INTERFACE_DISABLED" | "SCANNING" => "Disconnected".to_string(),
        _ => match ssid {
            Some(ssid) if !ssid.is_empty() => format!("Connecting to {ssid}"),
            _ => "Connecting".to_string(),
        },
    })
}

pub fn describe_iw(text: &str) -> String {
    if text.to_ascii_lowercase().contains("not connected") {
        return "Disconnected".to_string();
    }
    let mut ssid = None;
    let mut signal = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("SSID:") {
            ssid = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("signal:") {
            signal = Some(value.trim());
        }
    }
    match (ssid, signal) {
        (Some(ssid), Some(signal)) if !ssid.is_empty() => format!("Connected to {ssid} ({signal})"),
        (Some(ssid), None) if !ssid.is_empty() => format!("Connected to {ssid}"),
        _ if text.to_ascii_lowercase().contains("connected") => "Connected".to_string(),
        _ => "Disconnected".to_string(),
    }
}

fn wireless_interface() -> Option<String> {
    let entries = fs::read_dir("/sys/class/net").ok()?;
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if path.join("wireless").exists() || path.join("phy80211").exists() {
            names.push(name);
        }
    }
    names.sort();
    names.into_iter().next()
}

fn write_supplicant(config: &Config) -> Result<(), String> {
    let network = if config.wifi_password.is_empty() {
        format!(
            "network={{\n\tssid={}\n\tkey_mgmt=NONE\n\tscan_ssid=1\n}}\n",
            wpa_quote(&config.wifi_name)?
        )
    } else {
        format!(
            "network={{\n\tssid={}\n\tpsk={}\n\tscan_ssid=1\n}}\n",
            wpa_quote(&config.wifi_name)?,
            wpa_quote(&config.wifi_password)?
        )
    };
    let body = format!(
        "ctrl_interface=/run/wpa_supplicant\nupdate_config=0\n{network}"
    );
    write_private(Path::new(WPA_CONF), &body)
}

fn write_service_args(iface: &str) -> Result<(), String> {
    // Alpine's init script adds -B and the config file. It only needs the interface.
    let body = format!("wpa_supplicant_args=\"-i {iface}\"\n");
    write_private(Path::new(WPA_ARGS), &body)
}

fn ensure_dhcp(iface: &str) -> Result<(), String> {
    let path = Path::new("/etc/network/interfaces");
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("Cannot read {}: {err}", path.display())),
    };
    if existing
        .lines()
        .any(|line| line.split_whitespace().eq(["iface", iface, "inet", "dhcp"]))
    {
        return Ok(());
    }
    let mut body = existing;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&format!("\nauto {iface}\niface {iface} inet dhcp\n"));
    fs::write(path, body).map_err(|err| format!("Cannot write {}: {err}", path.display()))
}

fn write_private(path: &Path, body: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|err| format!("Cannot write {}: {err}", tmp.display()))?;
        file.write_all(body.as_bytes())
            .map_err(|err| format!("Cannot write {}: {err}", tmp.display()))?;
        file.sync_all()
            .map_err(|err| format!("Cannot write {}: {err}", tmp.display()))?;
    }
    fs::rename(&tmp, path).map_err(|err| format!("Cannot write {}: {err}", path.display()))?;
    Ok(())
}

fn wpa_quote(value: &str) -> Result<String, String> {
    let mut out = String::from("\"");
    for ch in value.chars() {
        if ch.is_control() {
            return Err("The Wi-Fi name or password contains a character that cannot be stored.".to_string());
        }
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    Ok(out)
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths)
            .any(|dir| dir.join(name).is_file())
    })
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wpa_status_describes_the_connection() {
        let text = "bssid=aa:bb\nssid=Shop Floor\nwpa_state=COMPLETED\n";
        assert_eq!(describe_wpa(text).as_deref(), Some("Connected to Shop Floor"));
        assert_eq!(
            describe_wpa("wpa_state=SCANNING\n").as_deref(),
            Some("Disconnected")
        );
        assert_eq!(
            describe_wpa("ssid=Shop Floor\nwpa_state=ASSOCIATING\n").as_deref(),
            Some("Connecting to Shop Floor")
        );
    }

    #[test]
    fn iw_link_describes_the_connection() {
        let text = "Connected to aa:bb:cc\n\tSSID: Shop\n\tsignal: -48 dBm\n";
        assert_eq!(describe_iw(text), "Connected to Shop (-48 dBm)");
        assert_eq!(describe_iw("Not connected.\n"), "Disconnected");
    }

    #[test]
    fn quotes_wpa_strings() {
        assert_eq!(wpa_quote(r#"a"b\c"#).unwrap(), r#""a\"b\\c""#);
    }
}
