//! Minimal geometry readers used to render Explorer thumbnails without a GPU or webview.
//!
//! Every reader converts into Babylon.js' left-handed, Y-up display space so a thumbnail
//! matches the default view of the same file in the viewer.

mod gltf;
mod obj;
mod ply;
mod splat;
mod stl;

/// A triangle mesh, or a point cloud when `indices` is empty.
#[derive(Default)]
pub struct Mesh {
    pub positions: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
    /// Optional per-vertex color, linear RGB.
    pub colors: Option<Vec<[f32; 3]>>,
    /// Linear RGB used when there are no vertex colors.
    pub base_color: [f32; 3],
}

impl Mesh {
    pub fn is_points(&self) -> bool {
        self.indices.is_empty()
    }
}

#[derive(Default)]
pub struct Model {
    pub meshes: Vec<Mesh>,
}

impl Model {
    pub fn is_empty(&self) -> bool {
        self.meshes.iter().all(|m| m.positions.is_empty())
    }
}

/// Neutral grey used for formats without materials (matches the viewer's default material).
pub const DEFAULT_COLOR: [f32; 3] = [0.36, 0.37, 0.39];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Stl,
    Obj,
    Gltf,
    Glb,
    Ply,
    Splat,
}

impl Format {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
            "stl" => Some(Self::Stl),
            "obj" => Some(Self::Obj),
            "gltf" => Some(Self::Gltf),
            "glb" => Some(Self::Glb),
            "ply" => Some(Self::Ply),
            "splat" => Some(Self::Splat),
            _ => None,
        }
    }

    /// Guesses the format from content, for streams that come without a file name.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"glTF") {
            return Some(Self::Glb);
        }
        if bytes.starts_with(b"ply") {
            return Some(Self::Ply);
        }
        if stl::looks_binary(bytes) {
            return Some(Self::Stl);
        }
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
        let trimmed = head.trim_start();
        if trimmed.starts_with("solid") && head.contains("facet") {
            return Some(Self::Stl);
        }
        if trimmed.starts_with('{') && head.contains("\"asset\"") {
            return Some(Self::Gltf);
        }
        if head.lines().any(|l| l.starts_with("v ")) {
            return Some(Self::Obj);
        }
        None
    }
}

/// Resolves external resources (e.g. a .gltf's .bin). Returns `None` when unavailable,
/// such as when Explorer hands us only a stream.
pub type ResourceLoader<'a> = &'a dyn Fn(&str) -> Option<Vec<u8>>;

pub fn load(bytes: &[u8], format: Format, resources: ResourceLoader) -> Result<Model, String> {
    let model = match format {
        Format::Stl => stl::parse(bytes)?,
        Format::Obj => obj::parse(bytes)?,
        Format::Gltf | Format::Glb => gltf::parse(bytes, resources)?,
        Format::Ply => ply::parse(bytes)?,
        Format::Splat => splat::parse(bytes)?,
    };
    if model.is_empty() {
        return Err("no geometry".into());
    }
    Ok(model)
}

pub(crate) fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_resources(_: &str) -> Option<Vec<u8>> {
        None
    }

    fn binary_stl(tris: &[[[f32; 3]; 3]]) -> Vec<u8> {
        let mut b = vec![0u8; 80];
        b.extend_from_slice(&(tris.len() as u32).to_le_bytes());
        for t in tris {
            b.extend_from_slice(&[0u8; 12]); // normal
            for v in t {
                for c in v {
                    b.extend_from_slice(&c.to_le_bytes());
                }
            }
            b.extend_from_slice(&[0, 0]);
        }
        b
    }

    #[test]
    fn stl_binary_swaps_y_and_z() {
        let bytes = binary_stl(&[[[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]]]);
        assert_eq!(Format::sniff(&bytes), Some(Format::Stl));
        let m = load(&bytes, Format::Stl, &no_resources).unwrap();
        assert_eq!(m.meshes[0].positions[0], [1.0, 3.0, 2.0]);
        assert_eq!(m.meshes[0].indices, vec![0, 1, 2]);
    }

    #[test]
    fn stl_binary_header_starting_with_solid() {
        // Some exporters write "solid" into the binary header; the size check must win.
        let mut bytes = binary_stl(&[[[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]]);
        bytes[..5].copy_from_slice(b"solid");
        let m = load(&bytes, Format::Stl, &no_resources).unwrap();
        assert_eq!(m.meshes[0].positions.len(), 3);
    }

    #[test]
    fn stl_ascii() {
        let text = "solid t\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\nendsolid t\n";
        assert_eq!(Format::sniff(text.as_bytes()), Some(Format::Stl));
        let m = load(text.as_bytes(), Format::Stl, &no_resources).unwrap();
        assert_eq!(
            m.meshes[0].positions,
            vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]
        );
    }

    #[test]
    fn obj_quads_and_negative_indices() {
        let text = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf -4/1 -3/2 -2/3 -1/4\n";
        let m = load(text.as_bytes(), Format::Obj, &no_resources).unwrap();
        assert_eq!(m.meshes[0].indices, vec![0, 1, 2, 0, 2, 3]);
        assert_eq!(m.meshes[0].positions[1], [-1.0, 0.0, 0.0]);
    }

    #[test]
    fn ply_binary_little_endian_with_colors() {
        let mut bytes = b"ply\nformat binary_little_endian 1.0\nelement vertex 3\nproperty float x\nproperty float y\nproperty float z\nproperty uchar red\nproperty uchar green\nproperty uchar blue\nelement face 1\nproperty list uchar int vertex_indices\nend_header\n".to_vec();
        for (p, c) in [
            ([0f32, 0., 0.], [255u8, 0, 0]),
            ([1., 0., 0.], [0, 255, 0]),
            ([0., 1., 0.], [0, 0, 255]),
        ] {
            for v in p {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            bytes.extend_from_slice(&c);
        }
        bytes.push(3);
        for i in [0i32, 1, 2] {
            bytes.extend_from_slice(&i.to_le_bytes());
        }
        let m = load(&bytes, Format::Ply, &no_resources).unwrap();
        let mesh = &m.meshes[0];
        assert_eq!(mesh.positions.len(), 3);
        assert_eq!(mesh.indices, vec![0, 1, 2]);
        assert_eq!(mesh.colors.as_ref().unwrap()[0], [1.0, 0.0, 0.0]);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(load(b"not a model", Format::Stl, &no_resources).is_err());
        assert_eq!(Format::sniff(b"\x00\x01\x02"), None);
    }

    #[test]
    fn renders_a_triangle() {
        let bytes = binary_stl(&[[[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 5.0]]]);
        let img = crate::render_bytes(&bytes, None, 64).unwrap();
        assert_eq!(img.bgra.len(), 64 * 64 * 4);
        assert!(img.bgra.chunks(4).any(|p| p[3] == 255));
        assert!(img.bgra.chunks(4).any(|p| p[3] == 0));
    }
}
