use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const DEFAULT_PATH: &str = "/var/lib/ctk/kiosk.conf";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub kiosk_code: String,
    pub wifi_name: String,
    pub wifi_password: String,
}

pub fn default_path() -> PathBuf {
    std::env::var_os("CTK_KIOSK_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_PATH))
}

impl Config {
    /// The configuration code is `kiosk-code.wifi-name.wifi-password`.
    /// The password is everything after the second dot, so it may contain dots.
    /// The kiosk code and Wi-Fi name cannot contain a dot.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim_matches(|ch: char| matches!(ch, '\r' | '\n' | '\0' | '\u{feff}'));
        let Some((kiosk_code, rest)) = raw.split_once('.') else {
            return Err(
                "The code must be kiosk-code.wifi-name.wifi-password.".to_string(),
            );
        };
        let Some((wifi_name, wifi_password)) = rest.split_once('.') else {
            return Err(
                "The code must be kiosk-code.wifi-name.wifi-password.".to_string(),
            );
        };
        if kiosk_code.is_empty() || wifi_name.is_empty() {
            return Err("The kiosk code and Wi-Fi name are required.".to_string());
        }
        if field_is_unsafe(kiosk_code) || field_is_unsafe(wifi_name) || field_is_unsafe(wifi_password)
        {
            return Err("The code contains a character that cannot be stored.".to_string());
        }
        Ok(Self {
            kiosk_code: kiosk_code.to_string(),
            wifi_name: wifi_name.to_string(),
            wifi_password: wifi_password.to_string(),
        })
    }

    pub fn load() -> Result<Option<Self>, String> {
        Self::load_from(&default_path())
    }

    pub fn load_from(path: &Path) -> Result<Option<Self>, String> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(format!("Cannot read {}: {err}", path.display())),
        };
        let mut kiosk_code = None;
        let mut wifi_name = None;
        let mut wifi_password = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("{} has an unreadable line.", path.display()));
            };
            match key.trim() {
                "kiosk-code" => kiosk_code = Some(value.to_string()),
                "wifi-name" => wifi_name = Some(value.to_string()),
                "wifi-password" => wifi_password = Some(value.to_string()),
                _ => {}
            }
        }
        match (kiosk_code, wifi_name, wifi_password) {
            (Some(kiosk_code), Some(wifi_name), Some(wifi_password))
                if !kiosk_code.is_empty() && !wifi_name.is_empty() =>
            {
                Ok(Some(Self {
                    kiosk_code,
                    wifi_name,
                    wifi_password,
                }))
            }
            _ => Err(format!("{} is missing the kiosk configuration.", path.display())),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&default_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
            }
        }
        let tmp = path.with_extension("conf.tmp");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|err| format!("Cannot write {}: {err}", tmp.display()))?;
        let body = format!(
            "kiosk-code={}\nwifi-name={}\nwifi-password={}\n",
            self.kiosk_code, self.wifi_name, self.wifi_password
        );
        file.write_all(body.as_bytes())
            .map_err(|err| format!("Cannot write {}: {err}", tmp.display()))?;
        file.sync_all()
            .map_err(|err| format!("Cannot write {}: {err}", tmp.display()))?;
        fs::rename(&tmp, path).map_err(|err| format!("Cannot write {}: {err}", path.display()))?;
        Ok(())
    }

    pub fn login_url(&self) -> String {
        format!(
            "https://mrp.ctkmanufacturing.com/v2/data-collection/account/login?kiosk-code={}",
            encode_component(&self.kiosk_code)
        )
    }
}

fn field_is_unsafe(value: &str) -> bool {
    value.chars().any(|ch| ch.is_control() || ch == '\n' || ch == '\r')
}

fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn parses_a_configuration_code() {
        let config = Config::parse("line-4.Shop Floor.p.a.ss").unwrap();
        assert_eq!(config.kiosk_code, "line-4");
        assert_eq!(config.wifi_name, "Shop Floor");
        assert_eq!(config.wifi_password, "p.a.ss");
    }

    #[test]
    fn rejects_a_code_without_three_fields() {
        assert!(Config::parse("only-code").is_err());
        assert!(Config::parse("code.wifi").is_err());
        assert!(Config::parse(".wifi.secret").is_err());
        assert!(Config::parse("code..secret").is_err());
    }

    #[test]
    fn allows_an_open_network() {
        let config = Config::parse("line-4.Shop.").unwrap();
        assert_eq!(config.wifi_password, "");
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("ctk-kiosk-config-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kiosk.conf");
        assert!(Config::load_from(&path).unwrap().is_none());
        let config = Config::parse("k1.Plant.secret=1").unwrap();
        config.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap().unwrap();
        assert_eq!(loaded, config);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn login_url_encodes_the_kiosk_code() {
        let config = Config::parse("a b.Shop.secret").unwrap();
        assert_eq!(
            config.login_url(),
            "https://mrp.ctkmanufacturing.com/v2/data-collection/account/login?kiosk-code=a%20b"
        );
    }
}
