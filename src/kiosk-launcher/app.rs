use crate::config::Config;

const COUNTDOWN_START: u8 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Screen {
    Countdown { remaining: u8 },
    Configure { scanning: bool, scan: String },
}

pub struct App {
    pub config: Option<Config>,
    pub screen: Screen,
    pub notice: String,
    pub wifi: String,
    launch: Option<String>,
    hold: bool,
    clock: bool,
    generation: u64,
}

impl App {
    pub fn start(config: Option<Config>, notice: String) -> Self {
        let clock = config.is_some() && notice.is_empty();
        let screen = if clock {
            Screen::Countdown {
                remaining: COUNTDOWN_START,
            }
        } else {
            Screen::Configure {
                scanning: false,
                scan: String::new(),
            }
        };
        Self {
            config,
            screen,
            notice,
            wifi: "Checking Wi-Fi...".to_string(),
            launch: None,
            hold: false,
            clock,
            generation: 1,
        }
    }

    pub fn take_clock(&mut self) -> bool {
        let armed = self.clock;
        self.clock = false;
        armed
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn set_wifi(&mut self, text: String) {
        if self.wifi != text {
            self.wifi = text;
            self.touch();
        }
    }

    pub fn tick(&mut self) {
        let Screen::Countdown { remaining } = self.screen else {
            return;
        };
        if self.hold || self.launch.is_some() {
            return;
        }
        let Some(config) = self.config.clone() else {
            return;
        };
        if remaining <= 1 {
            self.launch = Some(config.login_url());
            self.screen = Screen::Countdown { remaining: 0 };
        } else {
            self.screen = Screen::Countdown {
                remaining: remaining - 1,
            };
        }
        self.touch();
    }

    pub fn take_launch(&mut self) -> Option<String> {
        self.launch.take()
    }

    pub fn launch_failed(&mut self, error: String) {
        self.hold = true;
        self.notice = error;
        self.touch();
    }

    pub fn open_configuration(&mut self) {
        self.launch = None;
        self.hold = false;
        self.clock = false;
        self.notice.clear();
        self.screen = Screen::Configure {
            scanning: false,
            scan: String::new(),
        };
        self.touch();
    }

    pub fn begin_scan(&mut self) {
        self.notice.clear();
        self.screen = Screen::Configure {
            scanning: true,
            scan: String::new(),
        };
        self.touch();
    }

    pub fn cancel_scan(&mut self) {
        self.notice.clear();
        self.screen = Screen::Configure {
            scanning: false,
            scan: String::new(),
        };
        self.touch();
    }

    pub fn input_char(&mut self, ch: char) {
        let Screen::Configure {
            scanning: true,
            scan,
        } = &mut self.screen
        else {
            return;
        };
        if scan.chars().count() >= 1024 || ch.is_control() {
            return;
        }
        scan.push(ch);
        self.touch();
    }

    pub fn backspace(&mut self) {
        let Screen::Configure {
            scanning: true,
            scan,
        } = &mut self.screen
        else {
            return;
        };
        scan.pop();
        self.touch();
    }

    pub fn enter(&mut self) -> Option<Config> {
        let raw = match &self.screen {
            Screen::Configure {
                scanning: true,
                scan,
            } if !scan.is_empty() => scan.clone(),
            _ => return None,
        };
        match Config::parse(&raw) {
            Ok(config) => {
                self.screen = Screen::Configure {
                    scanning: false,
                    scan: String::new(),
                };
                self.notice.clear();
                self.touch();
                Some(config)
            }
            Err(err) => {
                self.notice = err;
                if let Screen::Configure { scan, .. } = &mut self.screen {
                    scan.clear();
                }
                self.touch();
                None
            }
        }
    }

    pub fn saved(&mut self, config: Config) {
        self.config = Some(config);
        self.hold = false;
        self.launch = None;
        self.clock = true;
        self.screen = Screen::Countdown {
            remaining: COUNTDOWN_START,
        };
        self.touch();
    }

    pub fn note(&mut self, notice: String) {
        self.notice = notice;
        self.touch();
    }

    pub fn press_action(&mut self) {
        match self.screen {
            Screen::Countdown { .. } => self.open_configuration(),
            Screen::Configure { scanning: false, .. } => self.begin_scan(),
            Screen::Configure { scanning: true, .. } => self.cancel_scan(),
        }
    }

    fn touch(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config::parse("line-4.Shop.secret").unwrap()
    }

    #[test]
    fn missing_configuration_opens_the_configuration_screen() {
        let app = App::start(None, String::new());
        assert_eq!(
            app.screen,
            Screen::Configure {
                scanning: false,
                scan: String::new()
            }
        );
    }

    #[test]
    fn countdown_reaches_chrome_after_five_seconds() {
        let mut app = App::start(Some(sample()), String::new());
        assert_eq!(app.screen, Screen::Countdown { remaining: 5 });
        for remaining in [4, 3, 2, 1] {
            app.tick();
            assert_eq!(app.screen, Screen::Countdown { remaining });
            assert!(app.take_launch().is_none());
        }
        app.tick();
        assert_eq!(
            app.take_launch().as_deref(),
            Some(
                "https://mrp.ctkmanufacturing.com/v2/data-collection/account/login?kiosk-code=line-4"
            )
        );
    }

    #[test]
    fn configuration_button_cancels_the_countdown() {
        let mut app = App::start(Some(sample()), String::new());
        app.tick();
        app.press_action();
        app.tick();
        app.tick();
        assert!(app.take_launch().is_none());
        assert!(matches!(app.screen, Screen::Configure { scanning: false, .. }));
    }

    #[test]
    fn scan_stores_the_code_and_returns_to_the_countdown() {
        let mut app = App::start(None, String::new());
        app.press_action();
        for ch in "line-4.Shop.secret".chars() {
            app.input_char(ch);
        }
        let config = app.enter().unwrap();
        assert_eq!(config.wifi_password, "secret");
        app.saved(config);
        assert_eq!(app.screen, Screen::Countdown { remaining: 5 });
    }

    #[test]
    fn cancel_leaves_the_scan() {
        let mut app = App::start(None, String::new());
        app.begin_scan();
        app.input_char('x');
        app.press_action();
        assert_eq!(
            app.screen,
            Screen::Configure {
                scanning: false,
                scan: String::new()
            }
        );
    }
}
