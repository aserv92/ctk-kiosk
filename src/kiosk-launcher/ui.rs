use std::io::Cursor;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use font8x8::{UnicodeFonts, BASIC_FONTS};
use x11rb::connection::Connection;
use x11rb::protocol::randr::{ConnectionExt as _, Rotation};
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux,
    EventMask, GrabMode, ImageFormat, ImageOrder, InputFocus, ModMask, Rectangle, Screen, StackMode,
    Visualtype, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::app::{App, PowerAction, Screen as KioskScreen};
use crate::config::Config;
use crate::screensaver;
use crate::sound;
use crate::wifi;

const XK_BACKSPACE: u32 = 0xff08;
const XK_RETURN: u32 = 0xff0d;
const XK_ESCAPE: u32 = 0xff1b;
const XK_KP_ENTER: u32 = 0xff8d;
const XK_SHIFT: u16 = 0x1;
const XK_XF86_POWER_DOWN: u32 = 0x1008_ff21;
const XK_XF86_POWER_OFF: u32 = 0x1008_ff2a;
// The Surface Pro 3 renders this scene entirely in software. A steady 30 FPS
// is smooth for its slow-moving animation and leaves CPU time for Chrome.
const SAVER_FRAME_TIME: Duration = Duration::from_millis(33);

pub fn run() -> Result<i32, String> {
    let (config, notice) = match Config::load() {
        Ok(config) => (config, String::new()),
        Err(err) => (None, err),
    };
    if let Some(config) = config.clone() {
        thread::spawn(move || {
            let _ = wifi::apply(&config);
        });
    }
    let mut app = App::start(config, notice);

    let (conn, screen_num) =
        x11rb::connect(None).map_err(|err| format!("Cannot open the display: {err}"))?;
    let screen = &conn.setup().roots[screen_num];
    let fallback_width = screen.width_in_pixels;
    let fallback_height = screen.height_in_pixels;
    let depth = screen.root_depth;
    let root = screen.root;
    let visual_id = screen.root_visual;
    let black = screen.black_pixel;
    let visual = find_visual(screen, visual_id).ok_or("The display has no usable visual.")?;
    let format = conn
        .setup()
        .pixmap_formats
        .iter()
        .find(|format| format.depth == depth)
        .ok_or("The display has no pixmap format for the root depth.")?;
    let order = conn.setup().image_byte_order;
    let bytes_per_pixel = match format.bits_per_pixel {
        16 => 2,
        24 => 3,
        32 => 4,
        other => {
            return Err(format!(
                "Unsupported display depth ({other} bits per pixel)."
            ))
        }
    };
    let scanline_pad = format.scanline_pad;
    let red_mask = visual.red_mask;
    let green_mask = visual.green_mask;
    let blue_mask = visual.blue_mask;
    let color_of = move |red, green, blue| Color {
        bytes: pack_pixel(order, red_mask, green_mask, blue_mask, red, green, blue),
        len: bytes_per_pixel,
    };

    use_best_resolution(&conn, root);
    hide_cursor(&conn, root);
    let (mut width, mut height) = display_size(&conn, root, fallback_width, fallback_height);

    let window = conn.generate_id().map_err(|err| err.to_string())?;
    conn.create_window(
        depth,
        window,
        root,
        0,
        0,
        width as u16,
        height as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        visual_id,
        &CreateWindowAux::new()
            .background_pixel(black)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::KEY_PRESS
                    | EventMask::POINTER_MOTION
                    | EventMask::STRUCTURE_NOTIFY,
            ),
    )
    .map_err(|err| err.to_string())?
    .check()
    .map_err(|err| err.to_string())?;

    let gc = conn.generate_id().map_err(|err| err.to_string())?;
    conn.create_gc(gc, window, &CreateGCAux::new().graphics_exposures(0))
        .map_err(|err| err.to_string())?
        .check()
        .map_err(|err| err.to_string())?;
    hide_cursor(&conn, window);
    conn.map_window(window)
        .map_err(|err| err.to_string())?
        .check()
        .map_err(|err| err.to_string())?;
    let _ = conn.set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME);
    conn.flush().map_err(|err| err.to_string())?;
    // The timeout stays on the X server, including after Chrome is in front.
    arm_screen_off(&conn);
    sound::play(&[sound::startup_cue(app.config.is_some())]);

    let keys = keyboard_map(&conn)?;
    grab_special_buttons(&conn, root, &keys);
    let logo = decode_logo(include_bytes!("ctk-logo.png")).unwrap_or_default();
    let mut frame = Frame::new(width, height, bytes_per_pixel, scanline_pad);
    let mut background =
        render_background(width, height, bytes_per_pixel, scanline_pad, &logo, &color_of);
    let max_request = conn.setup().maximum_request_length as usize * 4;
    let mut saver = screensaver::Saver::new();
    let mut saver_on = false;
    let mut manual = false;
    let mut last_windows = Instant::now() - Duration::from_secs(1);
    let mut window_shown = true;
    let mut last_input = Instant::now();
    let mut saver_from = Instant::now();
    let mut ignore_input_until = Instant::now();
    let mut awake_until = Instant::now();
    let mut server_idle_known: Option<bool> = None;
    let mut cached_server_idle = None;
    let mut next_server_idle = Instant::now();
    let mut seen = 0u64;
    let mut next_tick = Instant::now() + Duration::from_secs(1);
    let mut next_wifi = Instant::now();
    let mut chrome: Option<Child> = None;

    loop {
        let loop_started = Instant::now();
        while let Some(event) = conn.poll_for_event().map_err(|err| err.to_string())? {
            if let Event::KeyPress(press) = &event {
                let keysym = keys.keysym(press.detail, press.state.into());
                if power_button(keysym) {
                    last_input = Instant::now();
                    if saver_on {
                        saver_on = false;
                        manual = false;
                    }
                    app.toggle_power_menu();
                    continue;
                }
                if screensaver::windows_button(keysym) {
                    if last_windows.elapsed() >= Duration::from_millis(80) {
                        last_windows = Instant::now();
                        if saver_on {
                            saver_on = false;
                            manual = false;
                            ignore_input_until = Instant::now() + Duration::from_millis(400);
                            awake_until = Instant::now() + Duration::from_millis(500);
                            seen = 0;
                        } else {
                            saver_on = true;
                            manual = true;
                            saver_from = Instant::now();
                        }
                    }
                    continue;
                }
            }
            if input_event(&event) {
                last_input = Instant::now();
                if saver_on || Instant::now() < ignore_input_until {
                    if saver_on {
                        saver_on = false;
                        manual = false;
                        ignore_input_until = Instant::now() + Duration::from_millis(400);
                        awake_until = Instant::now() + Duration::from_millis(500);
                        seen = 0;
                    }
                    continue;
                }
            }
            match event {
                Event::ConfigureNotify(configure) => {
                    let next_width = u32::from(configure.width);
                    let next_height = u32::from(configure.height);
                    if next_width != width || next_height != height {
                        width = next_width;
                        height = next_height;
                        frame = Frame::new(width, height, bytes_per_pixel, scanline_pad);
                        background = render_background(
                            width,
                            height,
                            bytes_per_pixel,
                            scanline_pad,
                            &logo,
                            &color_of,
                        );
                        seen = 0;
                    }
                }
                Event::MapNotify(_) => {
                    let _ = conn.set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME);
                    conn.flush().map_err(|err| err.to_string())?;
                    seen = 0;
                }
                Event::Expose(expose) if expose.count == 0 => seen = 0,
                Event::ButtonPress(press) => {
                    on_click(&mut app, width, height, press.event_x as i32, press.event_y as i32);
                }
                Event::KeyPress(press) => {
                    on_key(&mut app, keys.keysym(press.detail, press.state.into()));
                }
                _ => {}
            }
        }

        if app.take_clock() {
            next_tick = Instant::now() + Duration::from_secs(1);
        }

        let now = Instant::now();
        if now >= next_tick {
            app.tick();
            next_tick = now + Duration::from_secs(1);
        }
        if matches!(app.screen, KioskScreen::Configure { .. }) && now >= next_wifi {
            app.set_wifi(wifi::status().text);
            next_wifi = now + Duration::from_secs(2);
        }

        if let Some(url) = app.take_launch() {
            match launch_chrome(&url) {
                Ok(child) => chrome = Some(child),
                Err(err) => app.launch_failed(err),
            }
        }
        if let Some(action) = app.take_power_action() {
            if let Err(err) = run_power_action(action) {
                app.note(err);
            }
        }
        if let Some(child) = chrome.as_mut() {
            // Chrome leaving ends this process so the session starts the launcher again.
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return Ok(0),
                Ok(None) => {}
            }
        }

        // This query waits for an X-server reply. Refreshing it once per second
        // is sufficient for a minute-long idle timeout and avoids a round trip
        // on every animation frame.
        if now >= next_server_idle {
            cached_server_idle = server_idle(&conn, root, &mut server_idle_known);
            next_server_idle = now + Duration::from_secs(1);
        }
        let server_idle = cached_server_idle;
        let chrome_running = chrome.is_some();
        let idle = if chrome_running {
            server_idle.unwrap_or(Duration::ZERO)
        } else {
            server_idle.unwrap_or_else(|| last_input.elapsed())
        };
        let can_save = !chrome_running || server_idle.is_some();
        if app.power_menu() {
            // Keep the menu visible when it was opened from the screen saver.
            saver_on = false;
            manual = false;
        } else if manual {
            saver_on = true;
        } else if now >= awake_until && screensaver::idle_ready(idle, can_save) {
            if !saver_on {
                saver_on = true;
                saver_from = Instant::now();
            }
        } else if now >= awake_until {
            saver_on = false;
        }

        let want_window = !chrome_running || saver_on || app.power_menu();
        if want_window != window_shown {
            set_window_shown(&conn, window, want_window)?;
            window_shown = want_window;
            if want_window {
                seen = 0;
            }
        }

        if saver_on && window_shown {
            let seconds = saver_from.elapsed().as_secs_f32();
            saver.render(width, height, seconds);
            frame.copy_rgb(saver.pixels(), saver.width(), saver.height(), &color_of);
            present(&conn, window, gc, depth, &frame, max_request)?;
        } else if (!chrome_running || app.power_menu()) && app.generation() != seen {
            paint(&mut frame, &background, &app, &color_of);
            present(&conn, window, gc, depth, &frame, max_request)?;
            seen = app.generation();
        }
        let delay = if saver_on {
            SAVER_FRAME_TIME.saturating_sub(loop_started.elapsed())
        } else {
            Duration::from_millis(30)
        };
        thread::sleep(delay);
    }
}

fn grab_special_buttons(conn: &RustConnection, root: u32, keys: &Keys) {
    let mut symbols = screensaver::windows_keysyms().to_vec();
    symbols.extend([XK_XF86_POWER_DOWN, XK_XF86_POWER_OFF]);
    for keycode in keys.keycodes_for(&symbols) {
        let _ = conn.grab_key(
            true,
            root,
            ModMask::ANY,
            keycode,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        );
    }
    let _ = conn.flush();
}

fn power_button(keysym: u32) -> bool {
    matches!(keysym, XK_XF86_POWER_DOWN | XK_XF86_POWER_OFF)
}

fn input_event(event: &Event) -> bool {
    matches!(
        event,
        Event::ButtonPress(_) | Event::KeyPress(_) | Event::MotionNotify(_)
    )
}

fn arm_screen_off(conn: &RustConnection) {
    use x11rb::protocol::dpms::ConnectionExt as _;
    let Ok(cookie) = conn.dpms_capable() else {
        return;
    };
    let Ok(reply) = cookie.reply() else {
        return;
    };
    if !reply.capable {
        return;
    }
    let seconds = screensaver::screen_off_seconds();
    if let Ok(cookie) = conn.dpms_set_timeouts(0, 0, seconds) {
        let _ = cookie.check();
    }
    if let Ok(cookie) = conn.dpms_enable() {
        let _ = cookie.check();
    }
    if let Ok(cookie) = conn.dpms_force_level(x11rb::protocol::dpms::DPMSMode::ON) {
        let _ = cookie.check();
    }
}

fn server_idle(
    conn: &RustConnection,
    root: u32,
    known: &mut Option<bool>,
) -> Option<Duration> {
    if *known == Some(false) {
        return None;
    }
    use x11rb::protocol::screensaver::ConnectionExt as _;
    let reply = conn
        .screensaver_query_info(root)
        .ok()
        .and_then(|cookie| cookie.reply().ok());
    let Some(reply) = reply else {
        *known = Some(false);
        return None;
    };
    *known = Some(true);
    Some(Duration::from_millis(u64::from(reply.ms_since_user_input)))
}

fn set_window_shown(conn: &RustConnection, window: u32, shown: bool) -> Result<(), String> {
    if shown {
        conn.map_window(window)
            .map_err(|err| err.to_string())?
            .check()
            .map_err(|err| err.to_string())?;
        conn.configure_window(
            window,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )
        .map_err(|err| err.to_string())?
        .check()
        .map_err(|err| err.to_string())?;
        let _ = conn.set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME);
    } else {
        conn.unmap_window(window)
            .map_err(|err| err.to_string())?
            .check()
            .map_err(|err| err.to_string())?;
        let _ = conn.set_input_focus(InputFocus::POINTER_ROOT, x11rb::NONE, x11rb::CURRENT_TIME);
    }
    conn.flush().map_err(|err| err.to_string())?;
    Ok(())
}

fn store_config(app: &mut App, config: Config) {
    if let Err(err) = config.save() {
        app.note(err);
        return;
    }
    let wifi_error = wifi::apply(&config).err();
    app.saved(config);
    if let Some(err) = wifi_error {
        app.note(err);
    }
}

fn on_click(app: &mut App, width: u32, height: u32, x: i32, y: i32) {
    let hits = layout(width, height);
    if app.power_menu() {
        if hits.power_off.contains(x, y) {
            app.power_off();
        } else if hits.restart.contains(x, y) {
            app.restart();
        } else {
            app.close_power_menu();
        }
        return;
    }
    if hits.action.contains(x, y) {
        app.press_action();
    }
}

fn on_key(app: &mut App, keysym: u32) {
    match keysym {
        XK_RETURN | XK_KP_ENTER => {
            let attempt = matches!(
                &app.screen,
                KioskScreen::Configure { scanning: true, scan } if !scan.is_empty()
            );
            if let Some(config) = app.enter() {
                if attempt {
                    sound::play(&sound::scan_cues(true));
                }
                store_config(app, config);
            } else if attempt {
                sound::play(&sound::scan_cues(false));
            }
        }
        XK_BACKSPACE => app.backspace(),
        XK_ESCAPE => {
            if app.power_menu() {
                app.close_power_menu();
            } else if matches!(app.screen, KioskScreen::Configure { scanning: true, .. }) {
                app.cancel_scan();
            }
        }
        sym if (0xffb0..=0xffb9).contains(&sym) => {
            if let Some(ch) = char::from_u32(u32::from('0') + (sym - 0xffb0)) {
                app.input_char(ch);
            }
        }
        sym if (0x20..=0x7e).contains(&sym) => {
            if let Some(ch) = char::from_u32(sym) {
                app.input_char(ch);
            }
        }
        _ => {}
    }
}

fn run_power_action(action: PowerAction) -> Result<(), String> {
    let command = match action {
        PowerAction::PowerOff => "poweroff",
        PowerAction::Restart => "reboot",
    };
    Command::new("systemctl")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("Could not {command} the kiosk: {err}"))
}

fn launch_chrome(url: &str) -> Result<Child, String> {
    let program = ["chromium-browser", "chromium", "google-chrome", "google-chrome-stable"]
        .into_iter()
        .find(|name| command_exists(name))
        .ok_or_else(|| "Chrome is not installed.".to_string())?;
    // Stay alive so the logo can cover Chrome and the panel can still sleep.
    Command::new(program)
        .args([
            "--kiosk",
            "--no-first-run",
            "--disable-infobars",
            "--disable-translate",
            "--disable-session-crashed-bubble",
            "--overscroll-history-navigation=0",
            "--check-for-update-interval=31536000",
            "--no-sandbox",
            "--user-data-dir=/var/lib/ctk/chrome",
            url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("Chrome did not start: {err}"))
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
    })
}

struct Keys {
    min: u8,
    per: u8,
    syms: Vec<u32>,
}

impl Keys {
    fn keycodes_for(&self, symbols: &[u32]) -> Vec<u8> {
        let per = usize::from(self.per.max(1));
        let mut codes = Vec::new();
        for (offset, row) in self.syms.chunks(per).enumerate() {
            if row.iter().any(|sym| *sym != 0 && symbols.contains(sym)) {
                let code = u16::from(self.min) + offset as u16;
                if code <= u16::from(u8::MAX) {
                    codes.push(code as u8);
                }
            }
        }
        codes
    }

    fn keysym(&self, keycode: u8, state: u16) -> u32 {
        let shifted = state & XK_SHIFT != 0;
        let level = if shifted && self.per > 1 { 1 } else { 0 };
        let index = keycode.wrapping_sub(self.min) as usize * self.per as usize + level;
        self.syms.get(index).copied().unwrap_or(0)
    }
}

struct Frame {
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
    stride: usize,
    data: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Color {
    bytes: [u8; 4],
    len: usize,
}

#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

struct Hits {
    scale: i32,
    action: Rect,
    power_off: Rect,
    restart: Rect,
}

fn layout(width: u32, height: u32) -> Hits {
    let width = width as i32;
    let height = height as i32;
    let scale = choose_scale(width, height);
    let margin = scale * 6;
    let line = scale * 10;
    let button_h = line + scale * 8;
    let menu_w = (width - margin * 2).min(scale * 36);
    let menu_x = (width - menu_w) / 2;
    let menu_y = (height - (line * 6 + button_h * 2 + scale * 3)) / 2;
    Hits {
        scale,
        action: Rect {
            x: margin,
            y: height - margin - button_h,
            w: width - margin * 2,
            h: button_h,
        },
        power_off: Rect {
            x: menu_x + scale * 2,
            y: menu_y + line * 3,
            w: menu_w - scale * 4,
            h: button_h,
        },
        restart: Rect {
            x: menu_x + scale * 2,
            y: menu_y + line * 3 + button_h + scale * 2,
            w: menu_w - scale * 4,
            h: button_h,
        },
    }
}

fn choose_scale(width: i32, height: i32) -> i32 {
    let mut scale = (height / 140).clamp(2, 8);
    while scale > 2 && 24 * 8 * scale + 12 * scale > width {
        scale -= 1;
    }
    scale
}

fn paint(frame: &mut Frame, background: &[u8], app: &App, color: &impl Fn(u8, u8, u8) -> Color) {
    let text = color(242, 244, 247);
    let muted = color(168, 176, 187);
    let button = color(61, 143, 110);
    let danger = color(196, 92, 74);
    let shadow = color(12, 14, 18);

    if background.len() == frame.data.len() {
        frame.data.copy_from_slice(background);
    } else {
        frame.fill(color(28, 31, 36));
    }
    let hits = layout(frame.width, frame.height);
    let scale = hits.scale;
    let margin = scale * 6;
    let line = scale * 10;
    let cols = ((frame.width as i32 - margin * 2) / (scale * 8)).max(8) as usize;

    draw_content(frame, margin, margin, scale, "CTK Kiosk", text, shadow);
    match &app.screen {
        KioskScreen::Countdown { remaining } => {
            draw_content(
                frame,
                margin,
                margin + line * 2,
                scale,
                "Data collection starts in",
                muted,
                shadow,
            );
            if app.notice.is_empty() && *remaining > 0 {
                let digit = remaining.to_string();
                let digit_scale = scale * 6;
                let width = digit.chars().count() as i32 * digit_scale * 8;
                let x = (frame.width as i32 - width) / 2;
                let y = (frame.height as i32 - digit_scale * 8) / 2;
                draw_content(frame, x, y, digit_scale, &digit, text, shadow);
            }
            let mut y = hits.action.y - line * 2;
            for row in wrap_lines(&app.notice, cols) {
                if row.is_empty() {
                    continue;
                }
                draw_content(frame, margin, y, scale, &row, danger, shadow);
                y += line;
            }
            button_label(frame, hits.action, scale, "Configuration", button, text);
        }
        KioskScreen::Configure { scanning, scan } => {
            let mut y = margin + line * 2;
            let intro = if *scanning {
                "Scan the configuration code."
            } else if app.config.is_some() {
                "Configuration is stored on this kiosk."
            } else {
                "This kiosk has no configuration yet."
            };
            for row in wrap_lines(intro, cols) {
                draw_content(frame, margin, y, scale, &row, text, shadow);
                y += line;
            }
            y += line / 2;
            let code = app
                .config
                .as_ref()
                .map(|config| format!("Kiosk code: {}", config.kiosk_code))
                .unwrap_or_else(|| "Kiosk code: none".to_string());
            draw_content(frame, margin, y, scale, &code, text, shadow);
            y += line * 2;
            draw_content(frame, margin, y, scale, "Wi-Fi", muted, shadow);
            y += line;
            for row in wrap_lines(&app.wifi, cols) {
                draw_content(frame, margin, y, scale, &row, text, shadow);
                y += line;
            }
            if *scanning {
                y += line / 2;
                let received = if scan.is_empty() {
                    "Waiting for the scanner.".to_string()
                } else {
                    format!("Received {} characters.", scan.chars().count())
                };
                draw_content(frame, margin, y, scale, &received, muted, shadow);
            }
            if !app.notice.is_empty() {
                y += line;
                for row in wrap_lines(&app.notice, cols) {
                    draw_content(frame, margin, y, scale, &row, danger, shadow);
                    y += line;
                }
            }
            let label = if *scanning {
                "Cancel"
            } else {
                "Scan configuration code"
            };
            button_label(frame, hits.action, scale, label, button, text);
        }
    }
    if app.power_menu() {
        draw_power_menu(frame, hits, text, button, danger, shadow);
    }
}

fn draw_power_menu(
    frame: &mut Frame,
    hits: Hits,
    text: Color,
    button: Color,
    danger: Color,
    shadow: Color,
) {
    frame.darken(3, 5);
    let pad = hits.scale * 2;
    let panel = Rect {
        x: hits.power_off.x - pad,
        y: hits.power_off.y - hits.scale * 3,
        w: hits.power_off.w + pad * 2,
        h: hits.restart.y + hits.restart.h - hits.power_off.y + hits.scale * 3 + pad,
    };
    frame.rect(panel, shadow);
    let title_width = "Power options".chars().count() as i32 * hits.scale * 8;
    draw_content(
        frame,
        panel.x + (panel.w - title_width) / 2,
        panel.y + hits.scale,
        hits.scale,
        "Power options",
        text,
        shadow,
    );
    button_label(frame, hits.power_off, hits.scale, "Power off", danger, text);
    button_label(frame, hits.restart, hits.scale, "Restart", button, text);
}

fn draw_content(
    frame: &mut Frame,
    x: i32,
    y: i32,
    scale: i32,
    text: &str,
    color: Color,
    shadow: Color,
) {
    draw_text(frame, x + scale, y + scale, scale, text, shadow);
    draw_text(frame, x, y, scale, text, color);
}

fn button_label(frame: &mut Frame, rect: Rect, scale: i32, label: &str, fill: Color, ink: Color) {
    frame.rect(rect, fill);
    let width = label.chars().count() as i32 * scale * 8;
    let x = rect.x + (rect.w - width).max(0) / 2;
    let y = rect.y + (rect.h - scale * 8) / 2;
    draw_text(frame, x, y, scale, label, ink);
}

fn draw_text(frame: &mut Frame, mut x: i32, y: i32, scale: i32, text: &str, color: Color) {
    for ch in text.chars() {
        if let Some(glyph) = BASIC_FONTS.get(ch) {
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..8 {
                    if bits & (1 << col) != 0 {
                        frame.rect(
                            Rect {
                                x: x + col * scale,
                                y: y + row as i32 * scale,
                                w: scale,
                                h: scale,
                            },
                            color,
                        );
                    }
                }
            }
        }
        x += scale * 8;
        if x >= frame.width as i32 {
            break;
        }
    }
}

fn wrap_lines(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.is_empty() {
            line = word.to_string();
        } else if line.chars().count() + 1 + word.chars().count() <= cols {
            line.push(' ');
            line.push_str(word);
        } else {
            lines.push(std::mem::take(&mut line));
            line = word.to_string();
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn present(
    conn: &RustConnection,
    window: u32,
    gc: u32,
    depth: u8,
    frame: &Frame,
    max_request: usize,
) -> Result<(), String> {
    let row_bytes = frame.stride.max(1);
    let max_rows = (max_request.saturating_sub(64) / row_bytes).clamp(1, frame.height as usize);
    let mut y = 0usize;
    while y < frame.height as usize {
        let rows = max_rows.min(frame.height as usize - y);
        let start = y * row_bytes;
        let end = (y + rows) * row_bytes;
        // A full-screen frame spans many X11 requests. Queue them all and
        // flush once instead of synchronously waiting for every chunk.
        conn.put_image(
            ImageFormat::Z_PIXMAP,
            window,
            gc,
            frame.width as u16,
            rows as u16,
            0,
            y as i16,
            0,
            depth,
            &frame.data[start..end],
        )
        .map_err(|err| err.to_string())?;
        y += rows;
    }
    conn.flush().map_err(|err| err.to_string())?;
    Ok(())
}

impl Frame {
    fn new(width: u32, height: u32, bytes_per_pixel: usize, scanline_pad: u8) -> Self {
        let pad = (scanline_pad as usize / 8).max(1);
        let raw = width as usize * bytes_per_pixel;
        let stride = raw.div_ceil(pad) * pad;
        Self {
            width,
            height,
            bytes_per_pixel,
            stride,
            data: vec![0; stride * height as usize],
        }
    }

    fn copy_rgb(
        &mut self,
        rgb: &[u8],
        src_w: u32,
        src_h: u32,
        color: &impl Fn(u8, u8, u8) -> Color,
    ) {
        if src_w == 0 || src_h == 0 || self.width == 0 || self.height == 0 {
            return;
        }
        for y in 0..self.height {
            let sy = (u64::from(y) * u64::from(src_h) / u64::from(self.height)) as u32;
            let row = sy.min(src_h - 1) as usize * src_w as usize * 3;
            for x in 0..self.width {
                let sx = (u64::from(x) * u64::from(src_w) / u64::from(self.width)) as u32;
                let index = row + sx.min(src_w - 1) as usize * 3;
                let Some(pixel) = rgb.get(index..index + 3) else {
                    return;
                };
                self.set_pixel(x as i32, y as i32, color(pixel[0], pixel[1], pixel[2]));
            }
        }
    }

    fn fill(&mut self, color: Color) {
        self.rect(
            Rect {
                x: 0,
                y: 0,
                w: self.width as i32,
                h: self.height as i32,
            },
            color,
        );
    }

    fn darken(&mut self, numerator: u8, denominator: u8) {
        if denominator == 0 || self.bytes_per_pixel < 3 {
            return;
        }
        for pixel in self.data.chunks_exact_mut(self.bytes_per_pixel) {
            for channel in pixel.iter_mut().take(3) {
                *channel = (u16::from(*channel) * u16::from(numerator)
                    / u16::from(denominator)) as u8;
            }
        }
    }

    fn rect(&mut self, rect: Rect, color: Color) {
        let x0 = rect.x.max(0) as usize;
        let y0 = rect.y.max(0) as usize;
        let x1 = (rect.x + rect.w).clamp(0, self.width as i32) as usize;
        let y1 = (rect.y + rect.h).clamp(0, self.height as i32) as usize;
        let len = color.len.min(self.bytes_per_pixel);
        for y in y0..y1 {
            for x in x0..x1 {
                let index = y * self.stride + x * self.bytes_per_pixel;
                self.data[index..index + len].copy_from_slice(&color.bytes[..len]);
            }
        }
    }

    fn set_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let index = y as usize * self.stride + x as usize * self.bytes_per_pixel;
        let len = color.len.min(self.bytes_per_pixel);
        self.data[index..index + len].copy_from_slice(&color.bytes[..len]);
    }
}

fn pack_pixel(
    order: ImageOrder,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    red: u8,
    green: u8,
    blue: u8,
) -> [u8; 4] {
    let pixel = channel(red_mask, red) | channel(green_mask, green) | channel(blue_mask, blue);
    match order {
        ImageOrder::MSB_FIRST => pixel.to_be_bytes(),
        _ => pixel.to_le_bytes(),
    }
}

fn channel(mask: u32, value: u8) -> u32 {
    if mask == 0 {
        return 0;
    }
    let bits = mask.count_ones();
    let shift = mask.trailing_zeros();
    let max = (1u32 << bits) - 1;
    ((u32::from(value) * max) / 255) << shift
}

fn find_visual(screen: &Screen, visual_id: u32) -> Option<&Visualtype> {
    screen.allowed_depths.iter().find_map(|depth| {
        if depth.depth == screen.root_depth {
            depth
                .visuals
                .iter()
                .find(|visual| visual.visual_id == visual_id)
        } else {
            None
        }
    })
}

fn keyboard_map(conn: &RustConnection) -> Result<Keys, String> {
    let min = conn.setup().min_keycode;
    let max = conn.setup().max_keycode;
    let count = max - min + 1;
    let reply = conn
        .get_keyboard_mapping(min, count)
        .map_err(|err| err.to_string())?
        .reply()
        .map_err(|err| err.to_string())?;
    Ok(Keys {
        min,
        per: reply.keysyms_per_keycode,
        syms: reply.keysyms,
    })
}

#[derive(Clone, Default)]
struct Logo {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

fn decode_logo(bytes: &[u8]) -> Result<Logo, String> {
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder.read_info().map_err(|err| err.to_string())?;
    let mut pixels = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut pixels).map_err(|err| err.to_string())?;
    if info.bit_depth != png::BitDepth::Eight || info.color_type != png::ColorType::Rgba {
        return Err("CTK logo is not an 8-bit RGBA image.".to_string());
    }
    pixels.truncate(info.buffer_size());
    Ok(Logo {
        width: info.width,
        height: info.height,
        pixels,
    })
}

fn render_background(
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
    scanline_pad: u8,
    logo: &Logo,
    color: &impl Fn(u8, u8, u8) -> Color,
) -> Vec<u8> {
    let mut frame = Frame::new(width, height, bytes_per_pixel, scanline_pad);
    let bg = (28, 31, 36);
    frame.fill(color(bg.0, bg.1, bg.2));
    blit_logo(&mut frame, logo, bg, color);
    frame.data
}

const LOGO_FIT_NUM: u32 = 4;
const LOGO_FIT_DEN: u32 = 5;
const LOGO_STRENGTH: u32 = 64;

fn blit_logo(frame: &mut Frame, logo: &Logo, bg: (u8, u8, u8), color: &impl Fn(u8, u8, u8) -> Color) {
    let (dest_w, dest_h) = logo_display_size(logo.width, logo.height, frame.width, frame.height);
    if dest_w == 0 || dest_h == 0 {
        return;
    }
    let origin_x = (frame.width as i32 - dest_w as i32) / 2;
    let origin_y = (frame.height as i32 - dest_h as i32) / 2;
    for y in 0..dest_h {
        for x in 0..dest_w {
            let src_x = (x as f32 + 0.5) * logo.width as f32 / dest_w as f32 - 0.5;
            let src_y = (y as f32 + 0.5) * logo.height as f32 / dest_h as f32 - 0.5;
            let sample = sample_bilinear(logo, src_x, src_y);
            if sample[3] == 0 {
                continue;
            }
            let alpha = logo_alpha(sample[3]);
            frame.set_pixel(
                origin_x + x as i32,
                origin_y + y as i32,
                color(
                    blend_channel(bg.0, sample[0], alpha),
                    blend_channel(bg.1, sample[1], alpha),
                    blend_channel(bg.2, sample[2], alpha),
                ),
            );
        }
    }
}

fn logo_display_size(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let (width, height) = contained_size(src_w, src_h, max_w, max_h);
    if width == 0 || height == 0 {
        return (0, 0);
    }
    (
        (u64::from(width) * u64::from(LOGO_FIT_NUM) / u64::from(LOGO_FIT_DEN)).max(1) as u32,
        (u64::from(height) * u64::from(LOGO_FIT_NUM) / u64::from(LOGO_FIT_DEN)).max(1) as u32,
    )
}

fn logo_alpha(alpha: u8) -> u8 {
    (u32::from(alpha) * LOGO_STRENGTH / 255) as u8
}

fn contained_size(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (0, 0);
    }
    if u64::from(src_w) * u64::from(max_h) >= u64::from(src_h) * u64::from(max_w) {
        let dest_w = max_w;
        let dest_h = (u64::from(max_w) * u64::from(src_h) / u64::from(src_w)).max(1) as u32;
        (dest_w, dest_h.min(max_h))
    } else {
        let dest_h = max_h;
        let dest_w = (u64::from(max_h) * u64::from(src_w) / u64::from(src_h)).max(1) as u32;
        (dest_w.min(max_w), dest_h)
    }
}

fn sample_bilinear(logo: &Logo, x: f32, y: f32) -> [u8; 4] {
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let tx = x - x0 as f32;
    let ty = y - y0 as f32;
    let c00 = logo_pixel(logo, x0, y0);
    let c10 = logo_pixel(logo, x0 + 1, y0);
    let c01 = logo_pixel(logo, x0, y0 + 1);
    let c11 = logo_pixel(logo, x0 + 1, y0 + 1);
    let mut out = [0u8; 4];
    for channel in 0..4 {
        let value = (1.0 - tx) * (1.0 - ty) * f32::from(c00[channel])
            + tx * (1.0 - ty) * f32::from(c10[channel])
            + (1.0 - tx) * ty * f32::from(c01[channel])
            + tx * ty * f32::from(c11[channel]);
        out[channel] = value.round().clamp(0.0, 255.0) as u8;
    }
    out
}

fn logo_pixel(logo: &Logo, x: i32, y: i32) -> [u8; 4] {
    if x < 0 || y < 0 || x >= logo.width as i32 || y >= logo.height as i32 {
        return [0, 0, 0, 0];
    }
    let index = (y as usize * logo.width as usize + x as usize) * 4;
    let Some(pixel) = logo.pixels.get(index..index + 4) else {
        return [0, 0, 0, 0];
    };
    [pixel[0], pixel[1], pixel[2], pixel[3]]
}

fn blend_channel(dst: u8, src: u8, alpha: u8) -> u8 {
    let alpha = u32::from(alpha);
    ((u32::from(src) * alpha + u32::from(dst) * (255 - alpha)) / 255) as u8
}

fn hide_cursor(conn: &RustConnection, window: u32) {
    let Ok(pixmap) = conn.generate_id() else {
        return;
    };
    let Ok(created) = conn.create_pixmap(1, pixmap, window, 1, 1) else {
        return;
    };
    if created.check().is_err() {
        return;
    }
    let Ok(gc) = conn.generate_id() else {
        return;
    };
    let Ok(created) = conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0)) else {
        return;
    };
    if created.check().is_err() {
        return;
    }
    // A cleared mask shows none of the cursor, so the pointer is invisible.
    let Ok(cleared) = conn.poly_fill_rectangle(
        pixmap,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        }],
    ) else {
        return;
    };
    if cleared.check().is_err() {
        return;
    }
    let _ = conn.free_gc(gc);
    let Ok(cursor) = conn.generate_id() else {
        return;
    };
    let Ok(created) = conn.create_cursor(cursor, pixmap, pixmap, 0, 0, 0, 0, 0, 0, 0, 0) else {
        return;
    };
    if created.check().is_err() {
        return;
    }
    let _ = conn.free_pixmap(pixmap);
    let aux = ChangeWindowAttributesAux::new().cursor(cursor);
    if let Ok(changed) = conn.change_window_attributes(window, &aux) {
        let _ = changed.check();
    }
    let _ = conn.free_cursor(cursor);
}

fn display_size(conn: &RustConnection, root: u32, fallback_width: u16, fallback_height: u16) -> (u32, u32) {
    conn.get_geometry(root)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|geometry| (u32::from(geometry.width), u32::from(geometry.height)))
        .unwrap_or((u32::from(fallback_width), u32::from(fallback_height)))
}

#[derive(Clone, Copy)]
struct ListedMode {
    id: u32,
    width: u16,
    height: u16,
    refresh: u32,
}

fn use_best_resolution(conn: &RustConnection, root: u32) {
    let Some(version) = conn.randr_query_version(1, 6).ok().and_then(|cookie| cookie.reply().ok()) else {
        return;
    };
    if version.major_version == 0 || (version.major_version == 1 && version.minor_version < 2) {
        return;
    }
    let Some(resources) = conn
        .randr_get_screen_resources_current(root)
        .ok().and_then(|cookie| cookie.reply().ok())
    else {
        return;
    };
    let Some(range) = conn
        .randr_get_screen_size_range(root)
        .ok().and_then(|cookie| cookie.reply().ok())
    else {
        return;
    };
    let Some(geometry) = conn.get_geometry(root).ok().and_then(|cookie| cookie.reply().ok()) else {
        return;
    };

    let modes: Vec<ListedMode> = resources.modes.iter().map(listed_mode).collect();
    let mut used = Vec::new();
    let mut placements = Vec::new();
    for &output in &resources.outputs {
        let Some(info) = conn
            .randr_get_output_info(output, resources.config_timestamp)
            .ok().and_then(|cookie| cookie.reply().ok())
        else {
            continue;
        };
        if info.connection != x11rb::protocol::randr::Connection::CONNECTED {
            continue;
        }
        let Some(crtc) = active_crtc(&info, &used) else {
            continue;
        };
        let Some(crtc_info) = conn
            .randr_get_crtc_info(crtc, resources.config_timestamp)
            .ok().and_then(|cookie| cookie.reply().ok())
        else {
            continue;
        };
        let rotation = if u16::from(crtc_info.rotation) == 0 {
            Rotation::ROTATE0
        } else {
            crtc_info.rotation
        };
        let Some(mode) = best_mode(
            &modes,
            &info.modes,
            rotation,
            range.min_width,
            range.min_height,
            range.max_width,
            range.max_height,
        ) else {
            continue;
        };
        let (width, height) = oriented(mode.width, mode.height, rotation);
        used.push(crtc);
        placements.push(Placement {
            output,
            crtc,
            mode: mode.id,
            x: 0,
            y: 0,
            width,
            height,
            rotation,
            mm_width: if info.mm_width == 0 {
                millimeters(width)
            } else {
                info.mm_width
            },
            mm_height: if info.mm_height == 0 {
                millimeters(height)
            } else {
                info.mm_height
            },
            was_mode: crtc_info.mode,
            was_x: crtc_info.x,
            was_y: crtc_info.y,
            was_width: crtc_info.width,
            was_height: crtc_info.height,
        });
    }
    layout_placements(&mut placements, range.max_width, range.max_height);
    if placements.is_empty() {
        return;
    }

    let screen_w = placements
        .iter()
        .map(|place| i32::from(place.x) + i32::from(place.width))
        .max()
        .unwrap_or(0);
    let screen_h = placements
        .iter()
        .map(|place| i32::from(place.y) + i32::from(place.height))
        .max()
        .unwrap_or(0);
    if screen_w <= 0
        || screen_h <= 0
        || screen_w > i32::from(range.max_width)
        || screen_h > i32::from(range.max_height)
    {
        return;
    }
    let screen_w = screen_w as u16;
    let screen_h = screen_h as u16;
    let same = screen_w == geometry.width
        && screen_h == geometry.height
        && placements.iter().all(|place| {
            place.was_mode == place.mode
                && place.was_x == place.x
                && place.was_y == place.y
                && place.was_width == place.width
                && place.was_height == place.height
        });
    if same {
        return;
    }

    let mm_w = placements
        .iter()
        .fold(0u32, |total, place| total.saturating_add(place.mm_width))
        .max(1);
    let mm_h = placements
        .iter()
        .map(|place| place.mm_height)
        .max()
        .unwrap_or(1)
        .max(1);
    // The framebuffer has to cover the new mode before it is programmed, then shrink afterward.
    let span_w = screen_w.max(geometry.width);
    let span_h = screen_h.max(geometry.height);
    let grew = span_w != geometry.width || span_h != geometry.height;
    if grew && !set_screen_size(conn, root, span_w, span_h, mm_w, mm_h) {
        return;
    }

    let mut failed = false;
    for place in &placements {
        let Some(reply) = conn
            .randr_set_crtc_config(
                place.crtc,
                x11rb::CURRENT_TIME,
                resources.config_timestamp,
                place.x,
                place.y,
                place.mode,
                place.rotation,
                &[place.output],
            )
            .ok().and_then(|cookie| cookie.reply().ok())
        else {
            failed = true;
            break;
        };
        if u8::from(reply.status) != u8::from(x11rb::protocol::randr::SetConfig::SUCCESS) {
            failed = true;
            break;
        }
    }
    if failed {
        if grew {
            let _ = set_screen_size(
                conn,
                root,
                geometry.width,
                geometry.height,
                millimeters(geometry.width),
                millimeters(geometry.height),
            );
        }
        return;
    }
    if screen_w != span_w || screen_h != span_h {
        let _ = set_screen_size(conn, root, screen_w, screen_h, mm_w, mm_h);
    }
}

struct Placement {
    output: u32,
    crtc: u32,
    mode: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    rotation: Rotation,
    mm_width: u32,
    mm_height: u32,
    was_mode: u32,
    was_x: i16,
    was_y: i16,
    was_width: u16,
    was_height: u16,
}

fn active_crtc(info: &x11rb::protocol::randr::GetOutputInfoReply, used: &[u32]) -> Option<u32> {
    let mut candidates = Vec::new();
    if info.crtc != 0 {
        candidates.push(info.crtc);
    }
    candidates.extend(info.crtcs.iter().copied().filter(|crtc| *crtc != 0));
    candidates.into_iter().find(|crtc| !used.contains(crtc))
}

fn listed_mode(mode: &x11rb::protocol::randr::ModeInfo) -> ListedMode {
    ListedMode {
        id: mode.id,
        width: mode.width,
        height: mode.height,
        refresh: refresh_rank(mode.dot_clock, mode.htotal, mode.vtotal),
    }
}

fn best_mode(
    modes: &[ListedMode],
    ids: &[u32],
    rotation: Rotation,
    min_width: u16,
    min_height: u16,
    max_width: u16,
    max_height: u16,
) -> Option<ListedMode> {
    ids.iter()
        .filter_map(|id| modes.iter().copied().find(|mode| mode.id == *id))
        .filter(|mode| {
            let (width, height) = oriented(mode.width, mode.height, rotation);
            mode.width > 0
                && mode.height > 0
                && width >= min_width
                && height >= min_height
                && width <= max_width
                && height <= max_height
        })
        .max_by_key(|mode| {
            (
                u32::from(mode.width) * u32::from(mode.height),
                mode.refresh,
                u32::from(mode.width),
            )
        })
}

fn oriented(width: u16, height: u16, rotation: Rotation) -> (u16, u16) {
    let bits = u16::from(rotation);
    let turned = bits & u16::from(Rotation::ROTATE90) != 0 || bits & u16::from(Rotation::ROTATE270) != 0;
    if turned {
        (height, width)
    } else {
        (width, height)
    }
}

fn refresh_rank(dot_clock: u32, htotal: u16, vtotal: u16) -> u32 {
    let total = u64::from(htotal) * u64::from(vtotal);
    if total == 0 {
        0
    } else {
        (u64::from(dot_clock) * 1000 / total) as u32
    }
}

fn millimeters(pixels: u16) -> u32 {
    (u32::from(pixels) * 254 / 960).max(1)
}

fn layout_placements(places: &mut Vec<Placement>, max_width: u16, max_height: u16) {
    let mut kept = Vec::new();
    let mut x = 0i32;
    for mut place in places.drain(..) {
        let next = x + i32::from(place.width);
        if next > i32::from(max_width) || i32::from(place.height) > i32::from(max_height) {
            if kept.is_empty() {
                continue;
            }
            break;
        }
        place.x = x as i16;
        place.y = 0;
        x = next;
        kept.push(place);
    }
    *places = kept;
}

fn set_screen_size(conn: &RustConnection, root: u32, width: u16, height: u16, mm_width: u32, mm_height: u32) -> bool {
    conn.randr_set_screen_size(root, width, height, mm_width, mm_height)
        .ok()
        .and_then(|cookie| cookie.check().ok())
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes() -> [ListedMode; 4] {
        [
            ListedMode {
                id: 1,
                width: 1280,
                height: 800,
                refresh: 60_000,
            },
            ListedMode {
                id: 2,
                width: 1920,
                height: 1080,
                refresh: 30_000,
            },
            ListedMode {
                id: 3,
                width: 1920,
                height: 1080,
                refresh: 60_000,
            },
            ListedMode {
                id: 4,
                width: 8000,
                height: 8000,
                refresh: 60_000,
            },
        ]
    }

    #[test]
    fn best_mode_picks_the_largest_area_then_refresh() {
        let best = best_mode(&modes(), &[1, 2, 3, 4], Rotation::ROTATE0, 1, 1, 4096, 4096).unwrap();
        assert_eq!(best.id, 3);
        assert!(best_mode(&modes(), &[], Rotation::ROTATE0, 1, 1, 100, 100).is_none());
    }

    #[test]
    fn best_mode_uses_the_rotated_size() {
        let modes = [ListedMode {
            id: 1,
            width: 2000,
            height: 1000,
            refresh: 1,
        }];
        assert!(best_mode(&modes, &[1], Rotation::ROTATE90, 1, 1, 1500, 2500).is_some());
        assert!(best_mode(&modes, &[1], Rotation::ROTATE90, 1, 1, 2500, 1500).is_none());
    }

    #[test]
    fn refresh_rank_uses_the_dot_clock() {
        assert_eq!(refresh_rank(148_500_000, 2200, 1125), 60_000);
        assert_eq!(refresh_rank(1, 0, 1), 0);
    }

    #[test]
    fn logo_fits_inside_the_screen() {
        assert_eq!(contained_size(512, 512, 2160, 1440), (1440, 1440));
        assert_eq!(logo_display_size(512, 512, 2160, 1440), (1152, 1152));
        assert_eq!(logo_display_size(512, 512, 100, 200), (80, 80));
        assert_eq!(logo_display_size(0, 10, 100, 100), (0, 0));
    }

    #[test]
    fn logo_is_drawn_dim_and_centered() {
        assert_eq!(logo_alpha(255), 64);
        assert_eq!(logo_alpha(0), 0);
        let swatch = Logo {
            width: 1,
            height: 1,
            pixels: vec![255, 0, 0, 255],
        };
        let color = |red, green, blue| Color {
            bytes: [red, green, blue, 255],
            len: 4,
        };
        let data = render_background(10, 10, 4, 32, &swatch, &color);
        let pixel = |x: usize, y: usize| {
            let index = y * 40 + x * 4;
            [data[index], data[index + 1], data[index + 2], data[index + 3]]
        };
        assert_eq!(pixel(0, 0), [28, 31, 36, 255]);
        assert_eq!(pixel(9, 9), [28, 31, 36, 255]);
        let center = pixel(5, 5);
        assert!(center[0] > 28 && center[0] < 160);
    }

    #[test]
    fn blend_keeps_the_background_when_transparent() {
        assert_eq!(blend_channel(28, 255, 0), 28);
        assert_eq!(blend_channel(28, 255, 255), 255);
        assert_eq!(blend_channel(0, 255, 128), 128);
    }

    #[test]
    fn logo_decodes() {
        let logo = decode_logo(include_bytes!("ctk-logo.png")).unwrap();
        assert_eq!((logo.width, logo.height), (512, 512));
        assert!(logo.pixels.chunks(4).any(|pixel| pixel[3] == 255));
    }
}
