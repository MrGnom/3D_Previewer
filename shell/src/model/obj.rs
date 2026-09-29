use super::{Mesh, Model, DEFAULT_COLOR};

/// Positions and faces only (materials live in a separate .mtl we may not be able to read).
/// Supports the common `v x y z r g b` vertex-color extension.
pub fn parse(bytes: &[u8]) -> Result<Model, String> {
    let text = String::from_utf8_lossy(bytes);
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut colors: Vec<[f32; 3]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut face: Vec<u32> = Vec::with_capacity(8);

    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("v ") {
            let n: Vec<f32> = rest
                .split_ascii_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if n.len() >= 3 {
                // Babylon's OBJ loader mirrors X to go from right- to left-handed.
                positions.push([-n[0], n[1], n[2]]);
                if n.len() >= 6 {
                    colors.push([n[3], n[4], n[5]]);
                }
            }
        } else if let Some(rest) = line.strip_prefix("f ") {
            face.clear();
            let count = positions.len() as i64;
            for tok in rest.split_ascii_whitespace() {
                let idx: i64 = match tok.split('/').next().and_then(|s| s.parse().ok()) {
                    Some(i) => i,
                    None => continue,
                };
                let resolved = if idx < 0 { count + idx } else { idx - 1 };
                if (0..count).contains(&resolved) {
                    face.push(resolved as u32);
                }
            }
            for i in 1..face.len().saturating_sub(1) {
                indices.extend_from_slice(&[face[0], face[i], face[i + 1]]);
            }
        }
    }

    if positions.is_empty() {
        return Err("OBJ has no vertices".into());
    }
    let colors = (colors.len() == positions.len()).then_some(colors);
    // A vertex-only OBJ (no faces) renders as a point cloud.
    Ok(Model {
        meshes: vec![Mesh {
            positions,
            indices,
            colors,
            base_color: DEFAULT_COLOR,
        }],
    })
}
