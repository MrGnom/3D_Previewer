//! Renders thumbnails with the same code path as the Explorer thumbnail provider.
//!
//! cargo run -p babylon-shell --example thumbnail -- <out-dir> <size> <model files...>
//!
//! STEP/IGES/BREP files need the kernel from `node scripts/build-occt.mjs`; it's picked up from
//! target/occt/occt.wasm unless `BABYLON_OCCT_WASM` points elsewhere. Set `BABYLON_SHELL_CACHE`
//! to a folder to cache the compiled kernel between runs (always on under Windows).

#[path = "common/png.rs"]
mod png;

use std::path::{Path, PathBuf};

use babylon_shell::{model::Format, render_bytes};

fn main() {
    if std::env::var_os("BABYLON_OCCT_WASM").is_none() {
        let built = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/occt/occt.wasm");
        std::env::set_var("BABYLON_OCCT_WASM", built);
    }
    let mut args = std::env::args().skip(1);
    let out_dir = PathBuf::from(
        args.next()
            .expect("usage: thumbnail <out-dir> <size> <files...>"),
    );
    let size: u32 = args.next().and_then(|s| s.parse().ok()).expect("size");
    std::fs::create_dir_all(&out_dir).unwrap();

    for file in args {
        let path = Path::new(&file);
        let bytes = std::fs::read(path).expect("read model");
        let format = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Format::from_extension);
        let started = std::time::Instant::now();
        match render_bytes(&bytes, format, size) {
            Some(img) => {
                let name = format!("{}.png", path.file_name().unwrap().to_string_lossy());
                let png = png::encode_bgra_premultiplied(
                    img.size,
                    img.size,
                    &img.bgra,
                    Some([255, 255, 255]),
                );
                std::fs::write(out_dir.join(&name), png).unwrap();
                println!("{file}: ok in {:?} -> {name}", started.elapsed());
            }
            None => println!("{file}: FAILED"),
        }
    }
}
