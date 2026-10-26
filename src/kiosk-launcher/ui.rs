use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use font8x8::{UnicodeFonts, BASIC_FONTS};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask,
    ImageFormat, ImageOrder, InputFocus, Screen, Visualtype, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::app::{App, Screen as KioskScreen};
use crate::config::Config;
use crate::wifi;

const XK_BACKSPACE: u32 = 0xff08;
const XK_RETURN: u32 = 0xff0d;
const XK_ESCAPE: u32 = 0xff1b;
const XK_KP_ENTER: u32 = 0xff8d;
const XK_SHIFT: u16 = 0x1;

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
    let mut width = u32::from(screen.width_in_pixels);
    let mut height = u32::from(screen.height_in_pixels);
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
    try_set_cursor(&conn, window);
    conn.map_window(window)
        .map_err(|err| err.to_string())?
        .check()
        .map_err(|err| err.to_string())?;
    let _ = conn.set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME);
    conn.flush().map_err(|err| err.to_string())?;

    let keys = keyboard_map(&conn)?;
    let mut frame = Frame::new(width, height, bytes_per_pixel, scanline_pad);
    let max_request = conn.setup().maximum_request_length as usize * 4;
    let mut seen = 0u64;
    let mut next_tick = Instant::now() + Duration::from_secs(1);
    let mut next_wifi = Instant::now();

    loop {
        while let Some(event) = conn.poll_for_event().map_err(|err| err.to_string())? {
            match event {
                Event::ConfigureNotify(configure) => {
                    let next_width = u32::from(configure.width);
                    let next_height = u32::from(configure.height);
                    if next_width != width || next_height != height {
                        width = next_width;
                        height = next_height;
                        frame = Frame::new(width, height, bytes_per_pixel, scanline_pad);
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
            if let Err(err) = launch_chrome(&url) {
                app.launch_failed(err);
            }
        }

        if app.generation() != seen {
            paint(&mut frame, &app, &color_of);
            present(&conn, window, gc, depth, &frame, max_request)?;
            seen = app.generation();
        }
        thread::sleep(Duration::from_millis(30));
    }
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
    if hits.action.contains(x, y) {
        app.press_action();
    }
}

fn on_key(app: &mut App, keysym: u32) {
    match keysym {
        XK_RETURN | XK_KP_ENTER => {
            if let Some(config) = app.enter() {
                store_config(app, config);
            }
        }
        XK_BACKSPACE => app.backspace(),
        XK_ESCAPE => {
            if matches!(app.screen, KioskScreen::Configure { scanning: true, .. }) {
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

fn launch_chrome(url: &str) -> Result<(), String> {
    let program = ["chromium-browser", "chromium", "google-chrome", "google-chrome-stable"]
        .into_iter()
        .find(|name| command_exists(name))
        .ok_or_else(|| "Chrome is not installed.".to_string())?;
    let mut command = Command::new(program);
    command.args([
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
    ]);
    let error = std::os::unix::process::CommandExt::exec(&mut command);
    Err(format!("Chrome did not start: {error}"))
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
}

fn layout(width: u32, height: u32) -> Hits {
    let width = width as i32;
    let height = height as i32;
    let scale = choose_scale(width, height);
    let margin = scale * 6;
    let line = scale * 10;
    let button_h = line + scale * 8;
    Hits {
        scale,
        action: Rect {
            x: margin,
            y: height - margin - button_h,
            w: width - margin * 2,
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

fn paint(frame: &mut Frame, app: &App, color: &impl Fn(u8, u8, u8) -> Color) {
    let bg = color(28, 31, 36);
    let text = color(242, 244, 247);
    let muted = color(168, 176, 187);
    let button = color(61, 143, 110);
    let danger = color(196, 92, 74);

    frame.fill(bg);
    let hits = layout(frame.width, frame.height);
    let scale = hits.scale;
    let margin = scale * 6;
    let line = scale * 10;
    let cols = ((frame.width as i32 - margin * 2) / (scale * 8)).max(8) as usize;

    draw_text(frame, margin, margin, scale, "CTK Kiosk", text);
    match &app.screen {
        KioskScreen::Countdown { remaining } => {
            draw_text(
                frame,
                margin,
                margin + line * 2,
                scale,
                "Data collection starts in",
                muted,
            );
            if app.notice.is_empty() && *remaining > 0 {
                let digit = remaining.to_string();
                let digit_scale = scale * 6;
                let width = digit.chars().count() as i32 * digit_scale * 8;
                let x = (frame.width as i32 - width) / 2;
                let y = (frame.height as i32 - digit_scale * 8) / 2;
                draw_text(frame, x, y, digit_scale, &digit, text);
            }
            let mut y = hits.action.y - line * 2;
            for row in wrap_lines(&app.notice, cols) {
                if row.is_empty() {
                    continue;
                }
                draw_text(frame, margin, y, scale, &row, danger);
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
                draw_text(frame, margin, y, scale, &row, text);
                y += line;
            }
            y += line / 2;
            let code = app
                .config
                .as_ref()
                .map(|config| format!("Kiosk code: {}", config.kiosk_code))
                .unwrap_or_else(|| "Kiosk code: none".to_string());
            draw_text(frame, margin, y, scale, &code, text);
            y += line * 2;
            draw_text(frame, margin, y, scale, "Wi-Fi", muted);
            y += line;
            for row in wrap_lines(&app.wifi, cols) {
                draw_text(frame, margin, y, scale, &row, text);
                y += line;
            }
            if *scanning {
                y += line / 2;
                let received = if scan.is_empty() {
                    "Waiting for the scanner.".to_string()
                } else {
                    format!("Received {} characters.", scan.chars().count())
                };
                draw_text(frame, margin, y, scale, &received, muted);
            }
            if !app.notice.is_empty() {
                y += line;
                for row in wrap_lines(&app.notice, cols) {
                    draw_text(frame, margin, y, scale, &row, danger);
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
        .map_err(|err| err.to_string())?
        .check()
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

fn try_set_cursor(conn: &RustConnection, window: u32) {
    let Ok(font) = conn.generate_id() else {
        return;
    };
    let Ok(opened) = conn.open_font(font, b"cursor") else {
        return;
    };
    if opened.check().is_err() {
        return;
    }
    let Ok(cursor) = conn.generate_id() else {
        return;
    };
    let Ok(created) =
        conn.create_glyph_cursor(cursor, font, font, 68, 69, 0, 0, 0, 65535, 65535, 65535)
    else {
        return;
    };
    if created.check().is_err() {
        return;
    }
    let aux = ChangeWindowAttributesAux::new().cursor(cursor);
    let _ = conn.change_window_attributes(window, &aux);
}
