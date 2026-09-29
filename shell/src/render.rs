//! Tiny CPU rasterizer for thumbnails: orthographic view from the viewer's default
//! camera angle, flat shading with a two-light studio rig, supersampled anti-aliasing,
//! transparent background.

use crate::model::Model;

/// Same default orbit as the viewer (`homeAlpha` / `homeBeta` in viewer.ts).
const ALPHA: f32 = -std::f32::consts::FRAC_PI_2 + 0.6;
const BETA: f32 = std::f32::consts::FRAC_PI_2 - 0.4;
const MARGIN: f32 = 0.06;

type V3 = [f32; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn normalize(a: V3) -> V3 {
    let l = dot(a, a).sqrt();
    if l > 0.0 {
        [a[0] / l, a[1] / l, a[2] / l]
    } else {
        [0.0, 0.0, -1.0]
    }
}

/// Premultiplied BGRA, top-down, `size`×`size`.
pub struct Image {
    pub size: u32,
    pub bgra: Vec<u8>,
}

struct Camera {
    right: V3,
    up: V3,
    forward: V3,
}

impl Camera {
    fn new() -> Self {
        // Babylon ArcRotateCamera: position = target + r * (cos α sin β, cos β, sin α sin β).
        let eye_dir = [
            ALPHA.cos() * BETA.sin(),
            BETA.cos(),
            ALPHA.sin() * BETA.sin(),
        ];
        let forward = normalize([-eye_dir[0], -eye_dir[1], -eye_dir[2]]);
        let right = normalize(cross([0.0, 1.0, 0.0], forward)); // left-handed
        let up = cross(forward, right);
        Self { right, up, forward }
    }

    fn view(&self, p: V3) -> V3 {
        [dot(p, self.right), dot(p, self.up), dot(p, self.forward)]
    }
}

fn shade(base: V3, n_view: V3) -> V3 {
    // Lights in view space (x right, y up, z into the screen); vectors point toward the light.
    let key = normalize([-0.45, 0.75, -0.55]);
    let fill = normalize([0.8, 0.1, -0.4]);
    let rim = normalize([0.2, 0.6, 0.8]);
    let view = [0.0, 0.0, -1.0];
    let n = if n_view[2] > 0.0 {
        [-n_view[0], -n_view[1], -n_view[2]]
    } else {
        n_view
    }; // two-sided
    let diffuse = 0.22
        + 0.85 * dot(n, key).max(0.0)
        + 0.3 * dot(n, fill).max(0.0)
        + 0.25 * dot(n, rim).max(0.0);
    let h = normalize([key[0] + view[0], key[1] + view[1], key[2] + view[2]]);
    let spec = 0.18 * dot(n, h).max(0.0).powf(48.0);
    [
        base[0] * diffuse + spec,
        base[1] * diffuse + spec,
        base[2] * diffuse + spec,
    ]
}

fn to_srgb8(linear: f32) -> f32 {
    // Soft shoulder so bright highlights don't clip harshly, then gamma encode.
    let x = linear / (1.0 + linear * 0.15);
    let s = if x <= 0.003_130_8 {
        x * 12.92
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    };
    (s.clamp(0.0, 1.0) * 255.0).round()
}

pub fn render(model: &Model, size: u32) -> Option<Image> {
    let size = size.clamp(16, 2048);
    let ss = if size <= 512 {
        3
    } else if size <= 1024 {
        2
    } else {
        1
    };
    let w = (size * ss) as usize;
    let cam = Camera::new();

    // Fit the projected extent into the frame.
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for m in &model.meshes {
        for &p in &m.positions {
            if !(p[0].is_finite() && p[1].is_finite() && p[2].is_finite()) {
                continue;
            }
            let v = cam.view(p);
            min_x = min_x.min(v[0]);
            max_x = max_x.max(v[0]);
            min_y = min_y.min(v[1]);
            max_y = max_y.max(v[1]);
        }
    }
    if min_x > max_x {
        return None;
    }
    let extent = (max_x - min_x).max(max_y - min_y).max(1e-12);
    let scale = w as f32 * (1.0 - 2.0 * MARGIN) / extent;
    let (cx, cy) = ((min_x + max_x) * 0.5, (min_y + max_y) * 0.5);
    let half = w as f32 * 0.5;
    let to_screen =
        |v: V3| -> V3 { [(v[0] - cx) * scale + half, half - (v[1] - cy) * scale, v[2]] };

    let mut depth = vec![f32::INFINITY; w * w];
    let mut color = vec![[0f32; 3]; w * w];

    for mesh in &model.meshes {
        let screen: Vec<V3> = mesh
            .positions
            .iter()
            .map(|&p| to_screen(cam.view(p)))
            .collect();
        let vertex_color = |i: usize| {
            mesh.colors
                .as_ref()
                .map(|c| c[i])
                .unwrap_or(mesh.base_color)
        };

        if mesh.is_points() {
            let radius = (w as f32 / 400.0).max(1.0);
            let r = radius.ceil() as i32;
            for (i, s) in screen.iter().enumerate() {
                if !s.iter().all(|c| c.is_finite()) {
                    continue;
                }
                let c = shade(vertex_color(i), [0.0, 0.3, -1.0]);
                let (px, py) = (s[0] as i32, s[1] as i32);
                for dy in -r..=r {
                    for dx in -r..=r {
                        let (x, y) = (px + dx, py + dy);
                        if x < 0 || y < 0 || x >= w as i32 || y >= w as i32 {
                            continue;
                        }
                        if ((dx * dx + dy * dy) as f32) > radius * radius {
                            continue;
                        }
                        let idx = y as usize * w + x as usize;
                        if s[2] < depth[idx] {
                            depth[idx] = s[2];
                            color[idx] = c;
                        }
                    }
                }
            }
            continue;
        }

        for tri in mesh.indices.chunks_exact(3) {
            let (i0, i1, i2) = (tri[0] as usize, tri[1] as usize, tri[2] as usize);
            let (a, b, c) = (screen[i0], screen[i1], screen[i2]);
            let area = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
            if area.abs() < 1e-12 || !area.is_finite() {
                continue;
            }
            let n_world = normalize(cross(
                sub(mesh.positions[i1], mesh.positions[i0]),
                sub(mesh.positions[i2], mesh.positions[i0]),
            ));
            let base = {
                let (c0, c1, c2) = (vertex_color(i0), vertex_color(i1), vertex_color(i2));
                [
                    (c0[0] + c1[0] + c2[0]) / 3.0,
                    (c0[1] + c1[1] + c2[1]) / 3.0,
                    (c0[2] + c1[2] + c2[2]) / 3.0,
                ]
            };
            let lit = shade(base, cam.view(n_world));

            let x0 = a[0].min(b[0]).min(c[0]).floor().max(0.0) as usize;
            let x1 = (a[0].max(b[0]).max(c[0]).ceil() as usize).min(w - 1);
            let y0 = a[1].min(b[1]).min(c[1]).floor().max(0.0) as usize;
            let y1 = (a[1].max(b[1]).max(c[1]).ceil() as usize).min(w - 1);
            if x0 > x1 || y0 > y1 {
                continue;
            }
            let inv = 1.0 / area;
            for y in y0..=y1 {
                let py = y as f32 + 0.5;
                for x in x0..=x1 {
                    let px = x as f32 + 0.5;
                    let w0 = ((b[0] - px) * (c[1] - py) - (b[1] - py) * (c[0] - px)) * inv;
                    let w1 = ((c[0] - px) * (a[1] - py) - (c[1] - py) * (a[0] - px)) * inv;
                    let w2 = 1.0 - w0 - w1;
                    if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                        continue;
                    }
                    let z = w0 * a[2] + w1 * b[2] + w2 * c[2];
                    let idx = y * w + x;
                    if z < depth[idx] {
                        depth[idx] = z;
                        color[idx] = lit;
                    }
                }
            }
        }
    }

    // Box-filter downsample to premultiplied BGRA.
    let n = size as usize;
    let ssu = ss as usize;
    let samples = (ssu * ssu) as f32;
    let mut bgra = vec![0u8; n * n * 4];
    let mut any = false;
    for y in 0..n {
        for x in 0..n {
            let (mut r, mut g, mut b, mut cov) = (0f32, 0f32, 0f32, 0f32);
            for sy in 0..ssu {
                for sx in 0..ssu {
                    let idx = (y * ssu + sy) * w + x * ssu + sx;
                    if depth[idx].is_finite() {
                        let c = color[idx];
                        r += to_srgb8(c[0]);
                        g += to_srgb8(c[1]);
                        b += to_srgb8(c[2]);
                        cov += 1.0;
                    }
                }
            }
            if cov > 0.0 {
                any = true;
                let o = (y * n + x) * 4;
                // Sum over covered samples / all samples == color × coverage (premultiplied).
                bgra[o] = (b / samples).round() as u8;
                bgra[o + 1] = (g / samples).round() as u8;
                bgra[o + 2] = (r / samples).round() as u8;
                bgra[o + 3] = (cov / samples * 255.0).round() as u8;
            }
        }
    }
    any.then_some(Image { size, bgra })
}
