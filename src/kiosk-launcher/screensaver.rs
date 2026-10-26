//! Idle scene for the kiosk and the installer. The session has no GPU
//! acceleration, so the plaque is rasterized into the framebuffer.

use std::io::Cursor;
use std::time::Duration;

const LOGO_PNG: &[u8] = include_bytes!("ctk-logo.png");
/// Surface Pro 3 Windows button. The kernel reports it as Left Meta, which X maps to Super.
const XK_SUPER_L: u32 = 0xffeb;
const XK_SUPER_R: u32 = 0xffec;
const XK_META_L: u32 = 0xffe7;
const XK_META_R: u32 = 0xffe8;
const PLATE: u32 = 256;
// This scene is software-rendered, including perspective-correct texture
// sampling. Rendering at this width preserves its detail when scaled for the
// display while fitting comfortably inside a Surface Pro 3 frame budget.
const BUFFER_WIDTH: u32 = 540;
pub const IDLE_AFTER: Duration = Duration::from_secs(60);
pub const SCREEN_OFF_AFTER: Duration = Duration::from_secs(60 * 60);

const HX: f32 = 1.05;
const HY: f32 = 1.05;
const HZ: f32 = 0.18;
const FLOOR_Y: f32 = -1.16;

pub struct Saver {
    plate: Vec<u8>,
    plate_size: u32,
    width: u32,
    height: u32,
    scratch: Vec<u8>,
    depth: Vec<f32>,
}

pub fn idle_ready(elapsed: Duration, choosing: bool) -> bool {
    choosing && elapsed >= IDLE_AFTER
}

pub fn windows_keysyms() -> [u32; 4] {
    [XK_SUPER_L, XK_SUPER_R, XK_META_L, XK_META_R]
}

pub fn windows_button(keysym: u32) -> bool {
    windows_keysyms().contains(&keysym)
}

/// DPMS off timeout in seconds. Zero is not used; the X server treats zero as disabled.
pub fn screen_off_seconds() -> u16 {
    SCREEN_OFF_AFTER.as_secs().min(u64::from(u16::MAX)) as u16
}

/// Project a point in plaque-local space. Screen Y grows downward.
pub fn screen_point(local: [f32; 3], seconds: f32, width: f32, height: f32) -> Option<[f32; 2]> {
    let cam = camera(seconds, height);
    let view = into_view(model(local, seconds), &cam);
    project_xy(view, &cam, width, height)
}

impl Saver {
    pub fn new() -> Self {
        let plate = match decode_logo(LOGO_PNG) {
            Ok((width, height, pixels)) => compose_plate(&Rgba {
                width,
                height,
                pixels,
            }),
            Err(_) => fallback_plate(),
        };
        Self::from_plate(plate)
    }

    pub fn render(&mut self, width: u32, height: u32, seconds: f32) {
        let width = width.max(1);
        let height = height.max(1);
        let (bw, bh) = buffer_size(width, height);
        let count = bw as usize * bh as usize;
        self.scratch.resize(count * 3, 0);
        self.depth.resize(count, 0.0);
        self.depth.fill(0.0);
        let cam = camera(seconds, bh as f32);
        background(&mut self.scratch, bw, bh, seconds);
        stars(&mut self.scratch, bw, bh, seconds);
        self.draw_plaque(bw, bh, seconds, &cam, true);
        self.draw_plaque(bw, bh, seconds, &cam, false);
        sparks(&mut self.scratch, &self.depth, bw, bh, seconds, &cam);
        vignette(&mut self.scratch, bw, bh);
        fade(&mut self.scratch, seconds);
        // Frame::copy_rgb already scales this image to the display. Avoid a
        // second full-resolution RGB buffer and a bilinear pass every frame.
        self.width = bw;
        self.height = bh;
    }

    pub fn pixels(&self) -> &[u8] {
        &self.scratch
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    fn from_plate(plate: Vec<u8>) -> Self {
        Self {
            plate,
            plate_size: PLATE,
            width: 0,
            height: 0,
            scratch: Vec::new(),
            depth: Vec::new(),
        }
    }

    fn draw_plaque(&mut self, width: u32, height: u32, seconds: f32, cam: &Camera, mirror: bool) {
        for tri in plaque() {
            let normal = surface_normal(tri.outward, seconds, mirror);
            // The camera looks along forward, so a face we can see points against it.
            if dot(normal, cam.forward) >= 0.0 {
                continue;
            }
            let shade = Shade {
                kind: tri.kind,
                light: dot(normal, light_dir(seconds)).max(0.0),
                fresnel: {
                    let facing = dot(normal, normalize(cam.eye)).max(0.0);
                    (1.0 - facing) * (1.0 - facing)
                },
                seconds,
            };
            let mut projected = [None, None, None];
            let mut visible = true;
            for (index, corner) in tri.corner.iter().enumerate() {
                let mut point = model(corner.p, seconds);
                if mirror {
                    point = reflect(point);
                }
                let view = into_view(point, cam);
                match project(view, corner.uv, cam, width as f32, height as f32) {
                    Some(vertex) => projected[index] = Some(vertex),
                    None => visible = false,
                }
            }
            if !visible {
                continue;
            }
            let a = projected[0].expect("vertex");
            let mut b = projected[1].expect("vertex");
            let mut c = projected[2].expect("vertex");
            // Screen Y is flipped, so normalize the winding before the edge test.
            if edge(&a, &b, &c) < 0.0 {
                std::mem::swap(&mut b, &mut c);
            }
            if edge(&a, &b, &c) < 0.5 {
                continue;
            }
            fill_tri(
                &mut self.scratch,
                &mut self.depth,
                width,
                height,
                a,
                b,
                c,
                &shade,
                &self.plate,
                self.plate_size,
                mirror,
            );
        }
    }

    #[cfg(test)]
    fn rgb_at(&self, x: u32, y: u32) -> [u8; 3] {
        let index = (y * self.width + x) as usize * 3;
        [
            self.scratch[index],
            self.scratch[index + 1],
            self.scratch[index + 2],
        ]
    }
}

struct Rgba {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Corner {
    p: [f32; 3],
    uv: [f32; 2],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FaceKind {
    Front,
    Back,
    Edge,
}

struct Tri {
    corner: [Corner; 3],
    kind: FaceKind,
    outward: [f32; 3],
}

struct Camera {
    eye: [f32; 3],
    right: [f32; 3],
    up: [f32; 3],
    forward: [f32; 3],
    focal: f32,
}

#[derive(Clone, Copy)]
struct Proj {
    x: f32,
    y: f32,
    inv_z: f32,
    u: f32,
    v: f32,
}

struct Shade {
    kind: FaceKind,
    light: f32,
    fresnel: f32,
    seconds: f32,
}

fn buffer_size(width: u32, height: u32) -> (u32, u32) {
    let width = width.max(2);
    let height = height.max(2);
    if width <= BUFFER_WIDTH {
        return (width, height);
    }
    let scaled = (u64::from(height) * u64::from(BUFFER_WIDTH) / u64::from(width)).max(2) as u32;
    (BUFFER_WIDTH, scaled)
}

fn decode_logo(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder.read_info().map_err(|err| err.to_string())?;
    let mut pixels = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut pixels).map_err(|err| err.to_string())?;
    if info.bit_depth != png::BitDepth::Eight || info.color_type != png::ColorType::Rgba {
        return Err("CTK logo is not an 8-bit RGBA image.".to_string());
    }
    pixels.truncate(info.buffer_size());
    Ok((info.width, info.height, pixels))
}

fn compose_plate(logo: &Rgba) -> Vec<u8> {
    let mut tex = vec![0u8; PLATE as usize * PLATE as usize * 4];
    for y in 0..PLATE {
        for x in 0..PLATE {
            let edge = x < 8 || y < 8 || x + 8 >= PLATE || y + 8 >= PLATE;
            let (r, g, b) = if edge {
                (26, 86, 70)
            } else {
                let v = y as f32 / (PLATE - 1) as f32;
                let grain = ((x.wrapping_mul(13) ^ y.wrapping_mul(7)) % 9) as f32;
                let base = 58.0 + v * 24.0;
                (
                    (base + grain * 0.7) as u8,
                    (base + 4.0 + grain * 0.4) as u8,
                    (base + 10.0) as u8,
                )
            };
            let index = (y * PLATE + x) as usize * 4;
            tex[index] = r;
            tex[index + 1] = g;
            tex[index + 2] = b;
        }
    }
    if logo.width == 0 || logo.height == 0 {
        return tex;
    }
    let margin = 18u32;
    let dest = PLATE - margin * 2;
    for y in 0..dest {
        for x in 0..dest {
            let sx = (x as f32 + 0.5) * logo.width as f32 / dest as f32 - 0.5;
            let sy = (y as f32 + 0.5) * logo.height as f32 / dest as f32 - 0.5;
            let sample = sample_rgba(&logo.pixels, logo.width, logo.height, sx, sy);
            if sample[3] == 0 {
                continue;
            }
            let index = ((margin + y) * PLATE + margin + x) as usize * 4;
            let alpha = u32::from(sample[3]);
            for channel in 0..3 {
                let dst = u32::from(tex[index + channel]);
                let src = u32::from(sample[channel]);
                tex[index + channel] = ((src * alpha + dst * (255 - alpha)) / 255) as u8;
            }
            tex[index + 3] = tex[index + 3].max(sample[3]);
        }
    }
    tex
}

fn fallback_plate() -> Vec<u8> {
    let mut tex = vec![0u8; PLATE as usize * PLATE as usize * 4];
    for y in 0..PLATE {
        for x in 0..PLATE {
            let dx = x as f32 - PLATE as f32 / 2.0;
            let dy = y as f32 - PLATE as f32 / 2.0;
            let dist = (dx * dx + dy * dy).sqrt();
            let ink = dist > 70.0 && dist < 96.0;
            let index = (y * PLATE + x) as usize * 4;
            if ink {
                tex[index] = 36;
                tex[index + 1] = 140;
                tex[index + 2] = 110;
                tex[index + 3] = 255;
            } else {
                tex[index] = 60;
                tex[index + 1] = 64;
                tex[index + 2] = 70;
            }
        }
    }
    tex
}

fn plaque() -> [Tri; 12] {
    let mut tris = Vec::with_capacity(12);
    push_quad(
        &mut tris,
        FaceKind::Front,
        [0.0, 0.0, 1.0],
        [
            corner([-HX, -HY, HZ], [0.0, 1.0]),
            corner([HX, -HY, HZ], [1.0, 1.0]),
            corner([HX, HY, HZ], [1.0, 0.0]),
            corner([-HX, HY, HZ], [0.0, 0.0]),
        ],
    );
    push_quad(
        &mut tris,
        FaceKind::Back,
        [0.0, 0.0, -1.0],
        [
            corner([HX, -HY, -HZ], [0.0, 1.0]),
            corner([-HX, -HY, -HZ], [1.0, 1.0]),
            corner([-HX, HY, -HZ], [1.0, 0.0]),
            corner([HX, HY, -HZ], [0.0, 0.0]),
        ],
    );
    push_quad(
        &mut tris,
        FaceKind::Edge,
        [1.0, 0.0, 0.0],
        [
            corner([HX, -HY, -HZ], [0.0, 1.0]),
            corner([HX, -HY, HZ], [1.0, 1.0]),
            corner([HX, HY, HZ], [1.0, 0.0]),
            corner([HX, HY, -HZ], [0.0, 0.0]),
        ],
    );
    push_quad(
        &mut tris,
        FaceKind::Edge,
        [-1.0, 0.0, 0.0],
        [
            corner([-HX, -HY, HZ], [0.0, 1.0]),
            corner([-HX, -HY, -HZ], [1.0, 1.0]),
            corner([-HX, HY, -HZ], [1.0, 0.0]),
            corner([-HX, HY, HZ], [0.0, 0.0]),
        ],
    );
    push_quad(
        &mut tris,
        FaceKind::Edge,
        [0.0, 1.0, 0.0],
        [
            corner([-HX, HY, HZ], [0.0, 1.0]),
            corner([HX, HY, HZ], [1.0, 1.0]),
            corner([HX, HY, -HZ], [1.0, 0.0]),
            corner([-HX, HY, -HZ], [0.0, 0.0]),
        ],
    );
    push_quad(
        &mut tris,
        FaceKind::Edge,
        [0.0, -1.0, 0.0],
        [
            corner([-HX, -HY, -HZ], [0.0, 1.0]),
            corner([HX, -HY, -HZ], [1.0, 1.0]),
            corner([HX, -HY, HZ], [1.0, 0.0]),
            corner([-HX, -HY, HZ], [0.0, 0.0]),
        ],
    );
    tris.try_into().ok().expect("plaque has 12 triangles")
}

fn corner(p: [f32; 3], uv: [f32; 2]) -> Corner {
    Corner { p, uv }
}

fn push_quad(tris: &mut Vec<Tri>, kind: FaceKind, outward: [f32; 3], corners: [Corner; 4]) {
    push_tri(tris, kind, outward, [corners[0], corners[1], corners[2]]);
    push_tri(tris, kind, outward, [corners[0], corners[2], corners[3]]);
}

fn push_tri(tris: &mut Vec<Tri>, kind: FaceKind, outward: [f32; 3], mut corner: [Corner; 3]) {
    let n = cross(
        sub(corner[1].p, corner[0].p),
        sub(corner[2].p, corner[0].p),
    );
    if dot(n, outward) < 0.0 {
        corner.swap(1, 2);
    }
    tris.push(Tri {
        corner,
        kind,
        outward,
    });
}

fn camera(seconds: f32, height: f32) -> Camera {
    let eye = [0.22 * (seconds * 0.19).sin(), 0.78, 3.30];
    let target = [0.0, -0.08, 0.0];
    let forward = normalize(sub(target, eye));
    let right = normalize(cross(forward, [0.0, 1.0, 0.0]));
    let up = cross(right, forward);
    Camera {
        eye,
        right,
        up,
        forward,
        focal: height * 1.05,
    }
}

fn model(p: [f32; 3], seconds: f32) -> [f32; 3] {
    let yaw = seconds * 0.28;
    let pitch = 0.48 * (seconds * 0.23).sin();
    let roll = 0.10 * (seconds * 0.17).sin();
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    let (sr, cr) = roll.sin_cos();
    let x1 = p[0] * cy + p[2] * sy;
    let z1 = -p[0] * sy + p[2] * cy;
    let y2 = p[1] * cp - z1 * sp;
    let z2 = p[1] * sp + z1 * cp;
    [x1 * cr - y2 * sr, x1 * sr + y2 * cr, z2]
}

fn surface_normal(outward: [f32; 3], seconds: f32, mirror: bool) -> [f32; 3] {
    let n = normalize(model(outward, seconds));
    if mirror { [n[0], -n[1], n[2]] } else { n }
}

fn reflect(p: [f32; 3]) -> [f32; 3] {
    [p[0], FLOOR_Y * 2.0 - p[1], p[2]]
}

fn light_dir(seconds: f32) -> [f32; 3] {
    normalize([
        (seconds * 0.65).sin() * 0.9,
        0.8,
        0.35 + (seconds * 0.65).cos() * 0.4,
    ])
}

fn into_view(p: [f32; 3], cam: &Camera) -> [f32; 3] {
    let d = sub(p, cam.eye);
    [dot(d, cam.right), dot(d, cam.up), dot(d, cam.forward)]
}

fn project(view: [f32; 3], uv: [f32; 2], cam: &Camera, width: f32, height: f32) -> Option<Proj> {
    if view[2] < 0.2 {
        return None;
    }
    let inv = 1.0 / view[2];
    let [x, y] = project_xy(view, cam, width, height)?;
    Some(Proj {
        x,
        y,
        inv_z: inv,
        u: uv[0],
        v: uv[1],
    })
}

fn project_xy(view: [f32; 3], cam: &Camera, width: f32, height: f32) -> Option<[f32; 2]> {
    if view[2] < 0.2 {
        return None;
    }
    let inv = 1.0 / view[2];
    Some([
        width * 0.5 + view[0] * cam.focal * inv,
        height * 0.5 - view[1] * cam.focal * inv,
    ])
}

fn edge(a: &Proj, b: &Proj, c: &Proj) -> f32 {
    (c.x - a.x) * (b.y - a.y) - (c.y - a.y) * (b.x - a.x)
}

fn edge_at(a: &Proj, b: &Proj, x: f32, y: f32) -> f32 {
    (x - a.x) * (b.y - a.y) - (y - a.y) * (b.x - a.x)
}

fn fill_tri(
    color: &mut [u8],
    depth: &mut [f32],
    width: u32,
    height: u32,
    a: Proj,
    b: Proj,
    c: Proj,
    shade: &Shade,
    plate: &[u8],
    plate_size: u32,
    mirror: bool,
) {
    let area = edge(&a, &b, &c);
    if area < 0.5 {
        return;
    }
    let min_x = a.x.min(b.x).min(c.x).floor().max(0.0) as i32;
    let max_x = a.x.max(b.x).max(c.x).ceil().min(width as f32 - 1.0) as i32;
    let min_y = a.y.min(b.y).min(c.y).floor().max(0.0) as i32;
    let max_y = a.y.max(b.y).max(c.y).ceil().min(height as f32 - 1.0) as i32;
    let horizon = height as f32 * 0.40;
    for y in min_y..=max_y {
        let py = y as f32 + 0.5;
        if mirror && py < horizon {
            continue;
        }
        for x in min_x..=max_x {
            let px = x as f32 + 0.5;
            let w0 = edge_at(&b, &c, px, py) / area;
            let w1 = edge_at(&c, &a, px, py) / area;
            let w2 = edge_at(&a, &b, px, py) / area;
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            let inv = w0 * a.inv_z + w1 * b.inv_z + w2 * c.inv_z;
            let index = y as usize * width as usize + x as usize;
            if inv <= depth[index] {
                continue;
            }
            let u = (w0 * a.u * a.inv_z + w1 * b.u * b.inv_z + w2 * c.u * c.inv_z) / inv;
            let v = (w0 * a.v * a.inv_z + w1 * b.v * b.inv_z + w2 * c.v * c.inv_z) / inv;
            let rgb = shade_pixel(shade, plate, plate_size, u, v, mirror, py, height as f32);
            depth[index] = inv;
            let pixel = index * 3;
            color[pixel] = rgb[0];
            color[pixel + 1] = rgb[1];
            color[pixel + 2] = rgb[2];
        }
    }
}

fn shade_pixel(
    shade: &Shade,
    plate: &[u8],
    plate_size: u32,
    u: f32,
    v: f32,
    mirror: bool,
    sy: f32,
    height: f32,
) -> [u8; 3] {
    let rgb = match shade.kind {
        FaceKind::Front => {
            let sample = sample_plate(plate, plate_size, u, v);
            let cover = sample[3] as f32 / 255.0;
            let lambert = 0.62 + 0.38 * shade.light;
            let sheen_u = 0.5 + 0.4 * (shade.seconds * 0.85).sin();
            let band = (1.0 - (u - sheen_u).abs() * 6.5).clamp(0.0, 1.0);
            let sheen = band * band * shade.light * (1.0 - cover);
            let spec = sheen * 110.0;
            [
                sample[0] as f32 * lambert + spec,
                sample[1] as f32 * lambert + spec,
                sample[2] as f32 * lambert + spec * 0.85,
            ]
        }
        FaceKind::Back => {
            let dx = u - 0.5;
            let dy = v - 0.5;
            let groove = ((dx * dx + dy * dy).sqrt() * 22.0).sin() * 0.5 + 0.5;
            let lambert = 0.16 + 0.84 * shade.light;
            let base = 54.0 + groove * 30.0;
            let fres = shade.fresnel;
            [
                base * lambert + fres * 18.0,
                base * lambert + fres * 48.0,
                base * 1.04 * lambert + fres * 36.0,
            ]
        }
        FaceKind::Edge => {
            let lambert = 0.12 + 0.88 * shade.light;
            let fres = shade.fresnel;
            [
                (132.0 + fres * 24.0) * lambert,
                (138.0 + fres * 78.0) * lambert,
                (146.0 + fres * 28.0) * lambert,
            ]
        }
    };
    let scale = if mirror {
        let t = ((sy - height * 0.40) / (height * 0.48)).clamp(0.0, 1.0);
        0.34 + t * 0.42
    } else {
        1.0
    };
    [
        (rgb[0] * scale).clamp(0.0, 255.0) as u8,
        (rgb[1] * scale).clamp(0.0, 255.0) as u8,
        (rgb[2] * scale).clamp(0.0, 255.0) as u8,
    ]
}

fn sample_plate(plate: &[u8], size: u32, u: f32, v: f32) -> [u8; 4] {
    let max = size.saturating_sub(1) as f32;
    sample_rgba(plate, size, size, u.clamp(0.0, 1.0) * max, v.clamp(0.0, 1.0) * max)
}

fn sample_rgba(pixels: &[u8], width: u32, height: u32, x: f32, y: f32) -> [u8; 4] {
    if width == 0 || height == 0 {
        return [0, 0, 0, 0];
    }
    let x = x.clamp(0.0, width.saturating_sub(1) as f32);
    let y = y.clamp(0.0, height.saturating_sub(1) as f32);
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let tx = x - x0 as f32;
    let ty = y - y0 as f32;
    let p00 = texel(pixels, width, x0, y0);
    let p10 = texel(pixels, width, x1, y0);
    let p01 = texel(pixels, width, x0, y1);
    let p11 = texel(pixels, width, x1, y1);
    let mut out = [0u8; 4];
    for channel in 0..4 {
        let value = (1.0 - tx) * (1.0 - ty) * f32::from(p00[channel])
            + tx * (1.0 - ty) * f32::from(p10[channel])
            + (1.0 - tx) * ty * f32::from(p01[channel])
            + tx * ty * f32::from(p11[channel]);
        out[channel] = value.round().clamp(0.0, 255.0) as u8;
    }
    out
}

fn texel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let index = (y * width + x) as usize * 4;
    [
        pixels[index],
        pixels[index + 1],
        pixels[index + 2],
        pixels[index + 3],
    ]
}

fn background(pixels: &mut [u8], width: u32, height: u32, seconds: f32) {
    let drift = seconds * 0.04;
    for y in 0..height {
        let ny = y as f32 / height as f32;
        let glow = (1.0 - (ny - 0.63).abs() * 3.4).clamp(0.0, 1.0);
        for x in 0..width {
            let nx = x as f32 / width as f32;
            let band = (nx * 0.35 + drift).fract();
            let aurora = (1.0 - (band - 0.5).abs() * 5.0).clamp(0.0, 1.0) * glow;
            let vign = 1.0
                - ((nx - 0.5) * (nx - 0.5) * 1.5 + (ny - 0.42) * (ny - 0.42) * 1.2).min(0.7);
            let index = (y * width + x) as usize * 3;
            pixels[index] = ((7.0 + glow * 12.0 + aurora * 20.0) * vign) as u8;
            pixels[index + 1] = ((10.0 + glow * 32.0 + aurora * 40.0) * vign) as u8;
            pixels[index + 2] = ((16.0 + glow * 18.0 + aurora * 16.0) * vign) as u8;
        }
    }
}

fn stars(pixels: &mut [u8], width: u32, height: u32, seconds: f32) {
    for i in 0..48u32 {
        let x = hash(i.wrapping_mul(19)) % width;
        let y = hash(i.wrapping_mul(47)) % height;
        let twinkle = 0.3 + 0.7 * (seconds * 1.5 + i as f32).sin().abs();
        add_pixel(pixels, width, height, x as i32, y as i32, [210, 220, 210], twinkle * 0.45);
    }
}

fn sparks(
    pixels: &mut [u8],
    depth: &[f32],
    width: u32,
    height: u32,
    seconds: f32,
    cam: &Camera,
) {
    for i in 0..24 {
        let angle = seconds * 0.45 + i as f32 * std::f32::consts::TAU / 24.0;
        let bob = (seconds * 0.8 + i as f32 * 0.7).sin() * 0.22;
        let radius = 1.62 + (i % 3) as f32 * 0.08;
        let point = [angle.cos() * radius, bob, angle.sin() * radius];
        let view = into_view(point, cam);
        let Some(projected) = project(view, [0.0, 0.0], cam, width as f32, height as f32) else {
            continue;
        };
        let x = projected.x.round() as i32;
        let y = projected.y.round() as i32;
        if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
            continue;
        }
        let index = y as usize * width as usize + x as usize;
        if projected.inv_z <= depth[index] {
            continue;
        }
        let color = if i % 2 == 0 {
            [150, 220, 186]
        } else {
            [230, 236, 214]
        };
        let gain = 0.75 + 0.45 * (projected.inv_z * 2.2).min(1.0);
        for dy in -2i32..=2 {
            for dx in -2i32..=2 {
                let dist = (dx * dx + dy * dy) as f32;
                if dist > 5.0 {
                    continue;
                }
                add_pixel(
                    pixels,
                    width,
                    height,
                    x + dx,
                    y + dy,
                    color,
                    gain * (1.0 - dist / 5.0),
                );
            }
        }
    }
}

fn add_pixel(pixels: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: [u8; 3], gain: f32) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let index = (y as usize * width as usize + x as usize) * 3;
    for channel in 0..3 {
        let value = f32::from(pixels[index + channel]) + f32::from(color[channel]) * gain;
        pixels[index + channel] = value.clamp(0.0, 255.0) as u8;
    }
}

fn vignette(pixels: &mut [u8], width: u32, height: u32) {
    for y in 0..height {
        let ny = y as f32 / height as f32 - 0.5;
        for x in 0..width {
            let nx = x as f32 / width as f32 - 0.5;
            let scale = 1.0 - (nx * nx * 1.05 + ny * ny * 0.95).min(0.5);
            let index = (y * width + x) as usize * 3;
            for channel in 0..3 {
                pixels[index + channel] = (f32::from(pixels[index + channel]) * scale) as u8;
            }
        }
    }
}

fn fade(pixels: &mut [u8], seconds: f32) {
    let t = (seconds / 0.85).clamp(0.0, 1.0);
    let amount = t * t * (3.0 - 2.0 * t);
    if amount >= 0.999 {
        return;
    }
    for pixel in pixels.iter_mut() {
        *pixel = (f32::from(*pixel) * amount) as u8;
    }
}

fn hash(n: u32) -> u32 {
    let mut x = n.wrapping_mul(2_246_822_519);
    x ^= x >> 13;
    x = x.wrapping_mul(3_266_489_917);
    x ^ (x >> 16)
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len < 1.0e-6 {
        [0.0, 0.0, 1.0]
    } else {
        [v[0] / len, v[1] / len, v[2] / len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(color: [u8; 4]) -> Saver {
        let mut plate = vec![0u8; PLATE as usize * PLATE as usize * 4];
        for pixel in plate.chunks_exact_mut(4) {
            pixel.copy_from_slice(&color);
        }
        Saver::from_plate(plate)
    }

    fn average(saver: &Saver, x0: u32, y0: u32, x1: u32, y1: u32) -> [u32; 3] {
        let mut acc = [0u32; 3];
        let mut count = 0u32;
        for y in y0..y1 {
            for x in x0..x1 {
                let pixel = saver.rgb_at(x, y);
                for channel in 0..3 {
                    acc[channel] += u32::from(pixel[channel]);
                }
                count += 1;
            }
        }
        [acc[0] / count, acc[1] / count, acc[2] / count]
    }

    #[test]
    fn the_tablet_windows_button_toggles_the_scene() {
        assert!(windows_button(0xffeb));
        assert!(windows_button(0xffec));
        assert!(windows_button(0xffe7));
        assert!(!windows_button(0xff0d));
        assert!(!windows_button(0xff1b));
    }

    #[test]
    fn idle_starts_after_a_minute() {
        assert!(!idle_ready(Duration::from_secs(59), true));
        assert!(idle_ready(Duration::from_secs(60), true));
        assert!(!idle_ready(Duration::from_secs(120), false));
    }

    #[test]
    fn the_panel_powers_off_after_an_hour() {
        assert_eq!(screen_off_seconds(), 3600);
        assert!(SCREEN_OFF_AFTER > IDLE_AFTER);
    }

    #[test]
    fn camera_matches_the_screen() {
        let right = screen_point([1.0, 0.0, 0.0], 0.0, 200.0, 100.0).unwrap();
        let left = screen_point([-1.0, 0.0, 0.0], 0.0, 200.0, 100.0).unwrap();
        assert!(right[0] > left[0] + 5.0, "right {right:?} left {left:?}");
        let up = screen_point([0.0, 1.0, 0.0], 0.0, 200.0, 100.0).unwrap();
        let down = screen_point([0.0, -1.0, 0.0], 0.0, 200.0, 100.0).unwrap();
        assert!(up[1] < down[1] - 5.0, "up {up:?} down {down:?}");
        let near = screen_point([1.0, 0.0, 0.5], 0.0, 200.0, 100.0).unwrap();
        let far = screen_point([1.0, 0.0, -0.5], 0.0, 200.0, 100.0).unwrap();
        assert!(near[0] > far[0], "near {near:?} far {far:?}");
        assert!(screen_point([0.0, 0.0, 20.0], 0.0, 200.0, 100.0).is_none());
    }

    #[test]
    fn front_of_a_red_plaque_covers_the_middle() {
        let mut saver = solid([220, 12, 12, 0]);
        saver.render(120, 80, 1.0);
        let center = average(&saver, 52, 32, 68, 48);
        let corner = average(&saver, 0, 0, 12, 12);
        assert!(
            center[0] > center[2] + 40,
            "center {center:?} corner {corner:?}"
        );
        assert!(corner[0] < 40, "corner {corner:?}");
    }

    #[test]
    fn the_turntable_moves() {
        let mut saver = solid([220, 12, 12, 0]);
        saver.render(80, 60, 1.0);
        let first = saver.pixels().to_vec();
        saver.render(80, 60, 3.6);
        assert_ne!(first, saver.pixels());
    }

    #[test]
    fn embedded_logo_is_the_ctk_mark() {
        let (width, height, pixels) = decode_logo(LOGO_PNG).unwrap();
        assert_eq!((width, height), (512, 512));
        assert!(pixels.chunks(4).any(|pixel| pixel[3] == 255));
        let saver = Saver::new();
        assert!(saver.plate.chunks(4).any(|pixel| pixel[3] > 200));
    }

    #[test]
    fn logo_is_on_the_face() {
        let mut saver = Saver::new();
        saver.render(160, 100, 1.05);
        let center = average(&saver, 60, 35, 100, 65);
        let corner = average(&saver, 0, 0, 16, 16);
        let diff = (center[0] as i32 - corner[0] as i32).abs()
            + (center[1] as i32 - corner[1] as i32).abs()
            + (center[2] as i32 - corner[2] as i32).abs();
        assert!(diff > 40, "center {center:?} corner {corner:?}");
    }
}
