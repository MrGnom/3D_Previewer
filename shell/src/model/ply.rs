use super::{srgb_to_linear, Mesh, Model, DEFAULT_COLOR};

#[derive(Clone, Copy, PartialEq)]
enum Encoding {
    Ascii,
    LittleEndian,
    BigEndian,
}

#[derive(Clone, Copy)]
enum Scalar {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl Scalar {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "char" | "int8" => Self::I8,
            "uchar" | "uint8" => Self::U8,
            "short" | "int16" => Self::I16,
            "ushort" | "uint16" => Self::U16,
            "int" | "int32" => Self::I32,
            "uint" | "uint32" => Self::U32,
            "float" | "float32" => Self::F32,
            "double" | "float64" => Self::F64,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }
}

struct Property {
    name: String,
    kind: Scalar,
    /// For list properties: the type of the element count.
    list_count: Option<Scalar>,
}

struct Element {
    name: String,
    count: usize,
    props: Vec<Property>,
}

/// Sequential reader over ASCII tokens or binary data.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    enc: Encoding,
}

impl Reader<'_> {
    fn next_token(&mut self) -> Option<&str> {
        let d = self.data;
        while self.pos < d.len() && d[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        let start = self.pos;
        while self.pos < d.len() && !d[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        (self.pos > start).then(|| std::str::from_utf8(&d[start..self.pos]).unwrap_or("0"))
    }

    fn read(&mut self, kind: Scalar) -> Option<f64> {
        if self.enc == Encoding::Ascii {
            return self.next_token()?.parse().ok();
        }
        let n = kind.size();
        let b = self.data.get(self.pos..self.pos + n)?;
        self.pos += n;
        let mut a = [0u8; 8];
        a[..n].copy_from_slice(b);
        if self.enc == Encoding::BigEndian {
            a[..n].reverse();
        }
        Some(match kind {
            Scalar::I8 => a[0] as i8 as f64,
            Scalar::U8 => a[0] as f64,
            Scalar::I16 => i16::from_le_bytes([a[0], a[1]]) as f64,
            Scalar::U16 => u16::from_le_bytes([a[0], a[1]]) as f64,
            Scalar::I32 => i32::from_le_bytes([a[0], a[1], a[2], a[3]]) as f64,
            Scalar::U32 => u32::from_le_bytes([a[0], a[1], a[2], a[3]]) as f64,
            Scalar::F32 => f32::from_le_bytes([a[0], a[1], a[2], a[3]]) as f64,
            Scalar::F64 => f64::from_le_bytes(a),
        })
    }
}

const SH_C0: f32 = 0.282_094_8;

pub fn parse(bytes: &[u8]) -> Result<Model, String> {
    let header_end = bytes
        .windows(10)
        .position(|w| w == b"end_header")
        .ok_or("PLY header not terminated")?;
    let header = String::from_utf8_lossy(&bytes[..header_end]);
    let mut body = header_end + 10;
    // Skip the line terminator after end_header (\n or \r\n), but no further: binary data follows.
    if bytes.get(body) == Some(&b'\r') {
        body += 1;
    }
    if bytes.get(body) == Some(&b'\n') {
        body += 1;
    }

    let mut enc = Encoding::Ascii;
    let mut elements: Vec<Element> = Vec::new();
    for line in header.lines() {
        let t: Vec<&str> = line.split_ascii_whitespace().collect();
        match t.as_slice() {
            ["format", f, ..] => {
                enc = match *f {
                    "ascii" => Encoding::Ascii,
                    "binary_little_endian" => Encoding::LittleEndian,
                    "binary_big_endian" => Encoding::BigEndian,
                    _ => return Err(format!("unknown PLY format {f}")),
                }
            }
            ["element", name, count] => elements.push(Element {
                name: name.to_string(),
                count: count.parse().map_err(|_| "bad PLY element count")?,
                props: Vec::new(),
            }),
            ["property", "list", count_kind, kind, name] => {
                let e = elements.last_mut().ok_or("PLY property before element")?;
                e.props.push(Property {
                    name: name.to_string(),
                    kind: Scalar::parse(kind).ok_or("bad PLY type")?,
                    list_count: Some(Scalar::parse(count_kind).ok_or("bad PLY type")?),
                });
            }
            ["property", kind, name] => {
                let e = elements.last_mut().ok_or("PLY property before element")?;
                e.props.push(Property {
                    name: name.to_string(),
                    kind: Scalar::parse(kind).ok_or("bad PLY type")?,
                    list_count: None,
                });
            }
            _ => {}
        }
    }

    let mut reader = Reader {
        data: bytes,
        pos: body,
        enc,
    };
    let mut positions = Vec::new();
    let mut colors = Vec::new();
    let mut indices = Vec::new();

    for element in &elements {
        let find = |n: &str| element.props.iter().position(|p| p.name == n);
        match element.name.as_str() {
            "vertex" => {
                let (ix, iy, iz) = (find("x"), find("y"), find("z"));
                let (Some(ix), Some(iy), Some(iz)) = (ix, iy, iz) else {
                    return Err("PLY vertex without x/y/z".into());
                };
                let rgb = [find("red"), find("green"), find("blue")];
                let dc = [find("f_dc_0"), find("f_dc_1"), find("f_dc_2")];
                let opacity = find("opacity");
                let is_splat = dc[0].is_some();
                let mut values = vec![0f64; element.props.len()];
                positions.reserve(element.count);
                for _ in 0..element.count {
                    for (i, p) in element.props.iter().enumerate() {
                        if let Some(ck) = p.list_count {
                            let n = reader.read(ck).ok_or("PLY truncated")? as usize;
                            for _ in 0..n {
                                reader.read(p.kind).ok_or("PLY truncated")?;
                            }
                        } else {
                            values[i] = reader.read(p.kind).ok_or("PLY truncated")?;
                        }
                    }
                    if let Some(o) = opacity {
                        // Splat opacity is stored as a logit; skip nearly transparent splats.
                        if 1.0 / (1.0 + (-values[o]).exp()) < 0.25 {
                            continue;
                        }
                    }
                    let (x, y, z) = (values[ix] as f32, values[iy] as f32, values[iz] as f32);
                    // Gaussian splat captures are conventionally Y-down.
                    positions.push(if is_splat { [x, -y, z] } else { [x, y, z] });
                    if let [Some(r), Some(g), Some(b)] = dc {
                        let f = |v: f64| srgb_to_linear((0.5 + SH_C0 * v as f32).clamp(0.0, 1.0));
                        colors.push([f(values[r]), f(values[g]), f(values[b])]);
                    } else if let [Some(r), Some(g), Some(b)] = rgb {
                        let scale = if matches!(element.props[r].kind, Scalar::F32 | Scalar::F64) {
                            1.0
                        } else {
                            255.0
                        };
                        let f = |v: f64| srgb_to_linear((v / scale).clamp(0.0, 1.0) as f32);
                        colors.push([f(values[r]), f(values[g]), f(values[b])]);
                    }
                }
            }
            "face" => {
                let list = element
                    .props
                    .iter()
                    .position(|p| p.name == "vertex_indices" || p.name == "vertex_index");
                for _ in 0..element.count {
                    for (i, p) in element.props.iter().enumerate() {
                        if let Some(ck) = p.list_count {
                            let n = reader.read(ck).ok_or("PLY truncated")? as usize;
                            let mut face = Vec::with_capacity(n);
                            for _ in 0..n {
                                face.push(reader.read(p.kind).ok_or("PLY truncated")? as u32);
                            }
                            if Some(i) == list {
                                for k in 1..n.saturating_sub(1) {
                                    indices.extend_from_slice(&[face[0], face[k], face[k + 1]]);
                                }
                            }
                        } else {
                            reader.read(p.kind).ok_or("PLY truncated")?;
                        }
                    }
                }
            }
            _ => {
                // Skip unknown elements; for binary data we still have to walk them.
                for _ in 0..element.count {
                    for p in &element.props {
                        if let Some(ck) = p.list_count {
                            let n = reader.read(ck).ok_or("PLY truncated")? as usize;
                            for _ in 0..n {
                                reader.read(p.kind).ok_or("PLY truncated")?;
                            }
                        } else {
                            reader.read(p.kind).ok_or("PLY truncated")?;
                        }
                    }
                }
            }
        }
    }

    let count = positions.len() as u32;
    indices.retain(|&i| i < count);
    indices.truncate(indices.len() / 3 * 3);
    let colors = (colors.len() == positions.len() && !colors.is_empty()).then_some(colors);
    Ok(Model {
        meshes: vec![Mesh {
            positions,
            indices,
            colors,
            base_color: DEFAULT_COLOR,
        }],
    })
}
