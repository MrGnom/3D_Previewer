use super::{Mesh, Model, DEFAULT_COLOR};

pub fn looks_binary(bytes: &[u8]) -> bool {
    if bytes.len() < 84 {
        return false;
    }
    let count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as u64;
    84 + count * 50 == bytes.len() as u64
}

/// STL is Z-up; Babylon's loader swaps Y and Z, and so do we.
fn to_display(x: f32, y: f32, z: f32) -> [f32; 3] {
    [x, z, y]
}

pub fn parse(bytes: &[u8]) -> Result<Model, String> {
    let is_ascii = !looks_binary(bytes) && {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
        head.trim_start().starts_with("solid")
    };
    let positions = if is_ascii {
        parse_ascii(bytes)
    } else {
        parse_binary(bytes)?
    };
    let indices = (0..positions.len() as u32).collect();
    Ok(Model {
        meshes: vec![Mesh {
            positions,
            indices,
            colors: None,
            base_color: DEFAULT_COLOR,
        }],
    })
}

fn parse_binary(bytes: &[u8]) -> Result<Vec<[f32; 3]>, String> {
    if bytes.len() < 84 {
        return Err("STL too short".into());
    }
    let declared = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
    // Trust the file length over the header when they disagree (truncated/odd exporters).
    let count = declared.min((bytes.len() - 84) / 50);
    let f = |o: usize| f32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    let mut positions = Vec::with_capacity(count * 3);
    for i in 0..count {
        let base = 84 + i * 50 + 12;
        for v in 0..3 {
            let o = base + v * 12;
            positions.push(to_display(f(o), f(o + 4), f(o + 8)));
        }
    }
    Ok(positions)
}

fn parse_ascii(bytes: &[u8]) -> Vec<[f32; 3]> {
    let text = String::from_utf8_lossy(bytes);
    let mut positions = Vec::new();
    for line in text.lines() {
        let mut it = line.split_ascii_whitespace();
        if it.next() == Some("vertex") {
            let mut v = [0f32; 3];
            for c in v.iter_mut() {
                *c = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            }
            positions.push(to_display(v[0], v[1], v[2]));
        }
    }
    positions.truncate(positions.len() / 3 * 3);
    positions
}
