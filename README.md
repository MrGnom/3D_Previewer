# Babylon Viewer (desktop)

A replacement for the retired Windows 3D Viewer, built on [Babylon.js](https://www.babylonjs.com/):

- **Desktop app** (Tauri 2): opens STL, OBJ, glTF/GLB, FBX, PLY, Gaussian splats (`.splat`, `.spz`, `.sog`), USD/USDZ, BVH and `.babylon` files, plus STEP (`.step`, `.stp`), IGES (`.iges`, `.igs`) and OpenCascade BREP CAD models. It supports double-click file associations, drag and drop, an Open dialog, a grid, wireframe mode, model info (size W×D×H, vertices, triangles) and animation playback.
- **Explorer Preview pane** (Alt+P): an interactive preview of the same formats. It's a native COM preview handler that hosts WebView2 and runs the same viewer bundle.
- **Explorer thumbnails**: STL, OBJ, glTF/GLB, PLY and `.splat` thumbnails, drawn by a small CPU rasterizer (no GPU or browser needed, ~10–50 ms per file).

```
viewer/       Babylon.js viewer (TypeScript + Vite), shared by the app and the preview pane
  src/stepLoader.ts  STEP/IGES/BREP loader plugin; OpenCascade (occt-import-js) runs in a Web Worker
src-tauri/    Desktop app: window, native Open dialog, model:// file protocol, installer config
shell/        babylon_shell.dll: preview handler + thumbnail provider (Rust, windows-rs, webview2-com)
  src/model/    geometry readers for thumbnails (STL, OBJ, PLY, glTF, splat)
  src/render.rs CPU rasterizer for thumbnails
  examples/     thumbnail + shelltest: exercise the DLL the way Explorer does
scripts/      make-samples.mjs (test models), register-dev.ps1 (dev registration)
samples/      generated test models
```

## Requirements

- Windows 10/11 x64 with the WebView2 runtime (preinstalled on Windows 11)
- Node.js 20+ and Rust (MSVC toolchain) with the Visual Studio C++ build tools

## Build

```bash
npm install
```

```bash
npm run build
```

This builds the viewer bundle and `babylon_shell.dll`, then the app, then the installer at
`target/release/bundle/nsis/Babylon Viewer_<version>_x64-setup.exe`. It's a per-user install, so no admin rights are needed. The installer:

1. installs the app plus `shell/babylon_shell.dll` and `viewer/` (the bundle the preview pane loads)
2. associates the file types with the app
3. runs `regsvr32` on the DLL to register the preview handler and thumbnail provider under HKCU

Uninstalling reverses all three.

## Development

```bash
npm run dev
```

This runs the app against the Vite dev server with hot reload. To work on the viewer alone in a browser:
`npm run dev --prefix viewer`, then open
`http://localhost:5173/?url=/@fs/C:/path/to/model.stl`. Add `&mode=preview` to see the preview-pane layout.

To try the Explorer extension without installing:

```powershell
./scripts/register-dev.ps1
```

```powershell
./scripts/register-dev.ps1 -Unregister
```

To test the DLL through COM without registering anything:

```bash
cargo run --release -p babylon-shell --example shelltest -- target/release/babylon_shell.dll out samples/box-and-ball.glb
```

The DLL must sit in the installed layout (`<dir>/shell/babylon_shell.dll` next to `<dir>/viewer/`).

```bash
cargo run --release -p babylon-shell --example thumbnail -- out 256 samples/torus-knot.stl
```

```bash
cargo test
```

### Troubleshooting the shell extension

- To turn on logging, create an empty file at `%USERPROFILE%\AppData\LocalLow\BabylonViewer\shell.log` (LocalLow, because Explorer runs preview handlers at low integrity), or set `BABYLON_SHELL_LOG` to a log path. The DLL then logs COM calls, served requests, WebView2 errors and the viewer's load result. Delete the file to turn logging off.
- `SHELLTEST_MTA=1` runs `shelltest` from an MTA thread, which reproduces how `prevhost.exe` calls preview handlers (neutral apartment on MTA threads).
- If a preview fails right after an uninstall/reinstall, sign out and back in: Windows' COM activation service can cache the removed registration.
- Explorer caches thumbnails. If old ones persist, clear them in Disk Cleanup → *Thumbnails*.
- If Microsoft 3D Viewer is still installed, Explorer uses its preview handler instead of ours: Store apps register handlers through their package manifest, and Explorer checks those first. Uninstall it (`Get-AppxPackage Microsoft.Microsoft3DViewer | Remove-AppxPackage`), then restart Explorer. Other apps that register handlers on a file type's ProgID can take precedence the same way.

## Known limitations

- Thumbnails show geometry and base colors only; textures are ignored. Draco- or meshopt-compressed glTF and `.gltf` files with external `.bin` buffers fall back to the default icon. The Preview pane shows all of them.
- Babylon.js fetches decoders for Draco, KTX2, meshopt and USD from its CDN on first use, so those files need a network connection. Plain STL, OBJ, glTF, FBX, PLY, splat and STEP/IGES/BREP files work fully offline.
- 3MF isn't supported yet, because Babylon.js has no 3MF loader.
- STEP, IGES and BREP files have no thumbnails yet (the CPU renderer has no CAD kernel); the Preview pane and the app show them fully. They're tessellated on open: finer in the app, coarser (faster) in the Preview pane. Sizes are shown in millimetres.

## CAD support and licensing

STEP/IGES/BREP files are read by [occt-import-js](https://github.com/kovacsv/occt-import-js), a WebAssembly build of [Open CASCADE Technology](https://dev.opencascade.org/). Both are licensed under the LGPL-2.1. The kernel ships unmodified as a separate file, `viewer/assets/occt-import-js-*.wasm`, which can be replaced with another build. Their license texts are installed in `licenses/`. It's only fetched when a CAD file is opened, so other formats load as fast as before.

