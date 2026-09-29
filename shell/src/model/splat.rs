use super::{srgb_to_linear, Mesh, Model, DEFAULT_COLOR};

/// `.splat`: packed 32-byte records of position (3×f32), scale (3×f32), RGBA (4×u8), rotation (4×u8).
pub fn parse(bytes: &[u8]) -> Result<Model, String> {
    const STRIDE: usize = 32;
    let count = bytes.len() / STRIDE;
    if count == 0 {
        return Err("empty splat file".into());
    }
    let mut positions = Vec::with_capacity(count);
    let mut colors = Vec::with_capacity(count);
    for rec in bytes.chunks_exact(STRIDE) {
        if rec[27] < 64 {
            continue; // nearly transparent
        }
        let f = |o: usize| f32::from_le_bytes([rec[o], rec[o + 1], rec[o + 2], rec[o + 3]]);
        // Splat captures are conventionally Y-down.
        positions.push([f(0), -f(4), f(8)]);
        let c = |v: u8| srgb_to_linear(v as f32 / 255.0);
        colors.push([c(rec[24]), c(rec[25]), c(rec[26])]);
    }
    Ok(Model {
        meshes: vec![Mesh {
            positions,
            indices: Vec::new(),
            colors: Some(colors),
            base_color: DEFAULT_COLOR,
        }],
    })
}
