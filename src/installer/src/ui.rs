use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use font8x8::{UnicodeFonts, BASIC_FONTS};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask,
    ImageFormat, ImageOrder, InputFocus, Screen, Visualtype, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::disks::{self, Disk};
use crate::install::{self, Progress};

const XK_RETURN: u32 = 0xff0d;
const XK_KP_ENTER: u32 = 0xff8d;
const XK_ESCAPE: u32 = 0xff1b;
const XK_UP: u32 = 0xff52;
const XK_DOWN: u32 = 0xff54;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Choose,
    Working,
    Done,
    Failed,
}

struct App {
    disks: Vec<Disk>,
    selected: Option<usize>,
    phase: Phase,
    error: String,
    exit: Option<i32>,
    progress: Arc<Progress>,
}

#[derive(Clone)]
struct View {
    disks: Vec<Disk>,
    selected: Option<usize>,
    phase: Phase,
    error: String,
    status: String,
    written: u64,
    total: u64,
}

struct Keys {
    min: u8,
    per: u8,
    syms: Vec<u32>,
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
    refresh: Rect,
    exit: Rect,
    install: Rect,
    reboot: Rect,
    back: Rect,
    disks: Vec<Rect>,
}

pub fn run() -> Result<i32, String> {
    let (conn, screen_num) =
        x11rb::connect(None).map_err(|err| format!("Cannot open the display: {err}"))?;
    let screen = &conn.setup().roots[screen_num];
    let width = u32::from(screen.width_in_pixels);
    let height = u32::from(screen.height_in_pixels);
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
    let app = Arc::new(Mutex::new(App {
        disks: disks::list_disks(),
        selected: None,
        phase: Phase::Choose,
        error: String::new(),
        exit: None,
        progress: Arc::new(Progress::new()),
    }));
    app.lock().expect("app lock").progress.touch();
    let mut seen = u64::MAX;

    loop {
        while let Some(event) = conn.poll_for_event().map_err(|err| err.to_string())? {
            handle_event(&conn, window, width, height, &keys, &app, event)?;
        }

        let exit_code = app.lock().expect("app lock").exit;
        if let Some(code) = exit_code {
            return Ok(code);
        }

        let generation = app.lock().expect("app lock").progress.generation();
        if generation != seen {
            let view = snapshot(&app.lock().expect("app lock"));
            let hits = layout(width, height, &view);
            paint(&mut frame, &view, &hits, &color_of);
            present(&conn, window, gc, depth, &frame, max_request)?;
            seen = generation;
        }
        thread::sleep(Duration::from_millis(30));
    }
}

fn handle_event(
    conn: &RustConnection,
    window: u32,
    width: u32,
    height: u32,
    keys: &Keys,
    app: &Arc<Mutex<App>>,
    event: Event,
) -> Result<(), String> {
    match event {
        Event::MapNotify(_) => {
            let _ = conn.set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME);
            conn.flush().map_err(|err| err.to_string())?;
            app.lock().expect("app lock").progress.touch();
        }
        Event::Expose(expose) if expose.count == 0 => {
            app.lock().expect("app lock").progress.touch();
        }
        Event::ButtonPress(press) => {
            let view = snapshot(&app.lock().expect("app lock"));
            let hits = layout(width, height, &view);
            on_click(app, &hits, press.event_x as i32, press.event_y as i32);
        }
        Event::KeyPress(press) => on_key(app, keys.keysym(press.detail)),
        _ => {}
    }
    Ok(())
}

fn snapshot(app: &App) -> View {
    View {
        disks: app.disks.clone(),
        selected: app.selected,
        phase: app.phase,
        error: app.error.clone(),
        status: app.progress.status(),
        written: app.progress.written(),
        total: app.progress.total(),
    }
}

fn on_click(app: &Arc<Mutex<App>>, hits: &Hits, x: i32, y: i32) {
    let mut guard = app.lock().expect("app lock");
    match guard.phase {
        Phase::Choose if hits.refresh.contains(x, y) => refresh(&mut guard),
        Phase::Choose if hits.exit.contains(x, y) => guard.exit = Some(1),
        Phase::Choose if hits.install.contains(x, y) => {
            drop(guard);
            start_install(app);
            return;
        }
        Phase::Choose => {
            if let Some(index) = hits.disks.iter().position(|rect| rect.contains(x, y)) {
                guard.selected = Some(index);
            } else {
                return;
            }
        }
        Phase::Failed if hits.back.contains(x, y) => {
            guard.phase = Phase::Choose;
            guard.error.clear();
        }
        Phase::Failed if hits.exit.contains(x, y) => guard.exit = Some(1),
        Phase::Done if hits.reboot.contains(x, y) => guard.exit = Some(0),
        Phase::Working | Phase::Failed | Phase::Done => return,
    }
    guard.progress.touch();
}

fn on_key(app: &Arc<Mutex<App>>, keysym: u32) {
    let mut guard = app.lock().expect("app lock");
    match guard.phase {
        Phase::Choose => match keysym {
            XK_UP => move_selection(&mut guard, -1),
            XK_DOWN => move_selection(&mut guard, 1),
            XK_RETURN | XK_KP_ENTER => {
                drop(guard);
                start_install(app);
                return;
            }
            XK_ESCAPE => guard.exit = Some(1),
            _ => return,
        },
        Phase::Failed if keysym == XK_ESCAPE => {
            guard.phase = Phase::Choose;
            guard.error.clear();
        }
        Phase::Done if keysym == XK_RETURN || keysym == XK_KP_ENTER => guard.exit = Some(0),
        _ => return,
    }
    guard.progress.touch();
}

fn move_selection(app: &mut App, delta: isize) {
    if app.disks.is_empty() {
        app.selected = None;
        return;
    }
    let next = match app.selected {
        Some(index) => index as isize + delta,
        None if delta < 0 => app.disks.len() as isize - 1,
        None => 0,
    };
    app.selected = Some(next.clamp(0, app.disks.len() as isize - 1) as usize);
}

fn refresh(app: &mut App) {
    let selected = app
        .selected
        .and_then(|index| app.disks.get(index).map(|disk| disk.path.clone()));
    app.disks = disks::list_disks();
    app.selected = selected.and_then(|path| app.disks.iter().position(|disk| disk.path == path));
}

fn start_install(app: &Arc<Mutex<App>>) {
    let mut guard = app.lock().expect("app lock");
    if guard.phase != Phase::Choose {
        return;
    }
    let Some(disk) = guard
        .selected
        .and_then(|index| guard.disks.get(index).map(|disk| disk.path.clone()))
    else {
        return;
    };
    guard.phase = Phase::Working;
    guard.error.clear();
    guard.progress.touch();
    let progress = Arc::clone(&guard.progress);
    drop(guard);

    let app = Arc::clone(app);
    thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            install::install_disk(&disk, &progress)
        }));
        let mut guard = app.lock().expect("app lock");
        match result {
            Ok(Ok(())) => guard.phase = Phase::Done,
            Ok(Err(err)) => {
                guard.error = err;
                guard.phase = Phase::Failed;
            }
            Err(_) => {
                guard.error =
                    "The installer hit an unexpected error while writing the disk.".to_string();
                guard.phase = Phase::Failed;
            }
        }
        guard.progress.touch();
    });
}

fn layout(width: u32, height: u32, view: &View) -> Hits {
    let width = width as i32;
    let height = height as i32;
    let scale = choose_scale(width, height, view.disks.len());
    let margin = scale * 6;
    let line = scale * 10;
    let button_h = line + scale * 4;
    let content_w = width - margin * 2;

    let refresh = Rect {
        x: width - margin - scale * 8 * 7,
        y: margin,
        w: scale * 8 * 7,
        h: line,
    };
    let bottom = Rect {
        x: margin,
        y: height - margin - button_h,
        w: content_w,
        h: button_h,
    };
    let exit = Rect {
        x: margin,
        y: bottom.y,
        w: content_w / 3,
        h: button_h,
    };
    let install = Rect {
        x: exit.x + exit.w + scale * 2,
        y: bottom.y,
        w: content_w - exit.w - scale * 2,
        h: button_h,
    };
    let back = Rect {
        x: margin,
        y: bottom.y,
        w: content_w / 2 - scale,
        h: button_h,
    };
    let exit_failed = Rect {
        x: back.x + back.w + scale * 2,
        y: bottom.y,
        w: content_w - back.w - scale * 2,
        h: button_h,
    };

    let mut disks = Vec::new();
    let card_h = line * 2 + scale * 4;
    let mut y = margin + line * 4;
    let list_bottom = bottom.y - line - scale * 2;
    for _ in &view.disks {
        if y + card_h >= list_bottom {
            break;
        }
        disks.push(Rect {
            x: margin,
            y,
            w: content_w,
            h: card_h,
        });
        y += card_h + scale * 2;
    }

    Hits {
        scale,
        refresh,
        exit: if view.phase == Phase::Failed {
            exit_failed
        } else {
            exit
        },
        install,
        reboot: bottom,
        back,
        disks,
    }
}

fn choose_scale(width: i32, height: i32, disks: usize) -> i32 {
    let mut scale = (height / 160).clamp(3, 6);
    while scale > 2 {
        let line = scale * 10;
        let button = line + scale * 4;
        let card = line * 2 + scale * 4;
        let header = scale * 6 + line * 4;
        let needed = header + disks as i32 * (card + scale * 2) + button + scale * 8;
        if needed <= height && scale * 8 * 42 <= width {
            break;
        }
        scale -= 1;
    }
    scale
}

fn paint(frame: &mut Frame, view: &View, hits: &Hits, color: &impl Fn(u8, u8, u8) -> Color) {
    let bg = color(28, 31, 36);
    let card = color(42, 48, 56);
    let card_on = color(36, 92, 69);
    let text = color(242, 244, 247);
    let muted = color(168, 176, 187);
    let button = color(61, 143, 110);
    let button_off = color(58, 65, 75);
    let danger = color(196, 92, 74);

    frame.fill(bg);
    let scale = hits.scale;
    let margin = scale * 6;
    let line = scale * 10;
    let cols = ((frame.width as i32 - margin * 2) / (scale * 8)).max(8) as usize;

    draw_text(frame, margin, margin, scale, "CTK Kiosk", text);
    let subtitle = match view.phase {
        Phase::Choose => "Select the drive to erase. Install writes the kiosk image and expands it to fill that drive.",
        Phase::Working => "Writing the kiosk image. Do not power off the tablet.",
        Phase::Done => "The kiosk image is installed. Unplug this USB drive, then reboot.",
        Phase::Failed => "The install did not finish.",
    };
    let mut y = margin + line;
    for row in wrap_lines(subtitle, cols) {
        draw_text(frame, margin, y, scale, &row, muted);
        y += line;
    }

    match view.phase {
        Phase::Choose => {
            button_label(frame, hits.refresh, scale, "Refresh", card, text);
            if view.disks.is_empty() {
                draw_text(
                    frame,
                    margin,
                    margin + line * 4,
                    scale,
                    "No other drives were found.",
                    text,
                );
            }
            for (index, disk) in view.disks.iter().enumerate() {
                let Some(rect) = hits.disks.get(index).copied() else {
                    break;
                };
                let on = view.selected == Some(index);
                frame.rect(rect, if on { card_on } else { card });
                let detail = format!(
                    "{}  {}  {}",
                    disks::human_size(disk.size),
                    disk.model,
                    disk.kind.label()
                );
                draw_text(frame, rect.x + scale * 2, rect.y + scale * 2, scale, &disk.path, text);
                draw_text(
                    frame,
                    rect.x + scale * 2,
                    rect.y + scale * 2 + line,
                    scale,
                    &detail,
                    if on { text } else { muted },
                );
            }
            let note = match view.selected.and_then(|index| view.disks.get(index)) {
                Some(disk) => format!(
                    "Install erases {}.",
                    disk.path
                ),
                None => "Select a drive, then press Install.".to_string(),
            };
            draw_text(frame, margin, hits.install.y - line - scale, scale, &note, text);
            button_label(
                frame,
                hits.install,
                scale,
                "Install",
                if view.selected.is_some() { button } else { button_off },
                text,
            );
            button_label(frame, hits.exit, scale, "Exit", card, text);
        }
        Phase::Working => {
            let y = margin + line * 4;
            draw_text(frame, margin, y, scale, &view.status, text);
            let bar = Rect {
                x: margin,
                y: y + line + scale * 2,
                w: frame.width as i32 - margin * 2,
                h: scale * 4,
            };
            frame.rect(bar, card);
            let fraction = if view.total == 0 {
                0.0
            } else {
                (view.written as f32 / view.total as f32).clamp(0.0, 1.0)
            };
            frame.rect(
                Rect {
                    x: bar.x,
                    y: bar.y,
                    w: (bar.w as f32 * fraction) as i32,
                    h: bar.h,
                },
                button,
            );
            let progress = if view.total == 0 {
                String::new()
            } else {
                format!(
                    "{} of {}",
                    disks::human_size(view.written),
                    disks::human_size(view.total)
                )
            };
            draw_text(frame, margin, bar.y + bar.h + scale * 2, scale, &progress, muted);
        }
        Phase::Done => button_label(frame, hits.reboot, scale, "Reboot", button, text),
        Phase::Failed => {
            let mut y = margin + line * 4;
            for row in wrap_lines(&view.error, cols) {
                draw_text(frame, margin, y, scale, &row, danger);
                y += line;
            }
            button_label(frame, hits.back, scale, "Back", card, text);
            button_label(frame, hits.exit, scale, "Exit", card, text);
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
    if lines.is_empty() {
        lines.push(String::new());
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

impl Keys {
    fn keysym(&self, keycode: u8) -> u32 {
        let index = keycode.wrapping_sub(self.min) as usize * self.per as usize;
        self.syms.get(index).copied().unwrap_or(0)
    }
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
    // Glyph 68 in the cursor font is the left pointer. Glyph 69 is its mask.
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
