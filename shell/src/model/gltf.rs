use base64::Engine as _;
use gltf::{accessor::DataType, buffer::Source, mesh::Mode, Semantic};

use super::{Mesh, Model, ResourceLoader};

type Mat4 = [[f32; 4]; 4]; // column-major, as glTF stores it

fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut r = [[0f32; 4]; 4];
    for (c, col) in r.iter_mut().enumerate() {
        for (row, out) in col.iter_mut().enumerate() {
            *out = (0..4).map(|k| a[k][row] * b[c][k]).sum();
        }
    }
    r
}

fn transform(m: &Mat4, p: [f32; 3]) -> [f32; 3] {
    let v = |r: usize| m[0][r] * p[0] + m[1][r] * p[1] + m[2][r] * p[2] + m[3][r];
    let w = v(3);
    let w = if w.abs() > 1e-12 { w } else { 1.0 };
    [v(0) / w, v(1) / w, v(2) / w]
}

const IDENTITY: Mat4 = [
    [1., 0., 0., 0.],
    [0., 1., 0., 0.],
    [0., 0., 1., 0.],
    [0., 0., 0., 1.],
];

fn decode_data_uri(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    if meta.ends_with(";base64") {
        base64::engine::general_purpose::STANDARD.decode(data).ok()
    } else {
        Some(percent_encoding::percent_decode_str(data).collect())
    }
}

/// Geometry and base colors only; textures are ignored. Draco/meshopt-compressed or
/// quantized primitives are skipped (the preview pane still shows them via Babylon.js).
pub fn parse(bytes: &[u8], resources: ResourceLoader) -> Result<Model, String> {
    let gltf = gltf::Gltf::from_slice_without_validation(bytes).map_err(|e| e.to_string())?;
    let doc = &gltf.document;

    let buffers: Vec<Option<Vec<u8>>> = doc
        .buffers()
        .map(|b| match b.source() {
            Source::Bin => gltf.blob.clone(),
            Source::Uri(uri) if uri.starts_with("data:") => decode_data_uri(uri),
            Source::Uri(uri) => {
                let decoded = percent_encoding::percent_decode_str(uri)
                    .decode_utf8_lossy()
                    .into_owned();
                resources(&decoded)
            }
        })
        .collect();

    let scene = doc
        .default_scene()
        .or_else(|| doc.scenes().next())
        .ok_or("glTF has no scene")?;
    let mut model = Model::default();
    let mut stack: Vec<(gltf::Node, Mat4)> = scene.nodes().map(|n| (n, IDENTITY)).collect();

    while let Some((node, parent)) = stack.pop() {
        let world = mul(&parent, &node.transform().matrix());
        for child in node.children() {
            stack.push((child, world));
        }
        let Some(mesh) = node.mesh() else { continue };
        for prim in mesh.primitives() {
            let mode = prim.mode();
            if !matches!(
                mode,
                Mode::Triangles | Mode::TriangleStrip | Mode::TriangleFan | Mode::Points
            ) {
                continue;
            }
            match prim.get(&Semantic::Positions) {
                Some(acc) if acc.data_type() == DataType::F32 && acc.view().is_some() => {}
                _ => continue,
            }
            let reader = prim.reader(|b| buffers.get(b.index()).and_then(|d| d.as_deref()));
            let Some(positions) = reader.read_positions() else {
                continue;
            };
            // glTF is right-handed; Babylon's loader ends up mirroring X.
            let positions: Vec<[f32; 3]> = positions
                .map(|p| {
                    let [x, y, z] = transform(&world, p);
                    [-x, y, z]
                })
                .collect();
            let n = positions.len() as u32;

            let raw: Vec<u32> = match reader.read_indices() {
                Some(i) => i.into_u32().collect(),
                None if mode == Mode::Points => Vec::new(),
                None => (0..n).collect(),
            };
            let mut indices = match mode {
                Mode::Triangles => raw,
                Mode::TriangleStrip => (2..raw.len())
                    .flat_map(|i| {
                        if i % 2 == 0 {
                            [raw[i - 2], raw[i - 1], raw[i]]
                        } else {
                            [raw[i - 1], raw[i - 2], raw[i]]
                        }
                    })
                    .collect(),
                Mode::TriangleFan => (2..raw.len())
                    .flat_map(|i| [raw[0], raw[i - 1], raw[i]])
                    .collect(),
                _ => Vec::new(),
            };
            if indices.iter().any(|&i| i >= n) {
                continue;
            }
            indices.truncate(indices.len() / 3 * 3);

            let colors = reader
                .read_colors(0)
                .map(|c| c.into_rgb_f32().collect::<Vec<_>>())
                .filter(|c| c.len() == positions.len());
            let [r, g, b, _] = prim.material().pbr_metallic_roughness().base_color_factor();
            let colors = colors.map(|c| {
                c.into_iter()
                    .map(|v| [v[0] * r, v[1] * g, v[2] * b])
                    .collect()
            });
            model.meshes.push(Mesh {
                positions,
                indices,
                colors,
                base_color: [r, g, b],
            });
        }
    }
    Ok(model)
}
