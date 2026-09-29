//! Exercises babylon_shell.dll through COM the way Explorer does, without registering it.
//!
//! cargo run -p babylon-shell --example shelltest -- <dll> <out-dir> <model file>
//!
//! Writes `<out-dir>/thumbnail.png` (IThumbnailProvider via IInitializeWithStream) and
//! `<out-dir>/preview.png` (IPreviewHandler hosted in a window, captured after loading).

#[path = "common/png.rs"]
mod png;

use std::{
    ffi::c_void,
    path::PathBuf,
    time::{Duration, Instant},
};

use windows::{
    core::{w, Interface, GUID, HRESULT, HSTRING, PCSTR},
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC,
            DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, SelectObject, BITMAP,
            BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, SRCCOPY,
        },
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, IClassFactory, IStream, CLSCTX_INPROC_SERVER,
                CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, STGM_READ,
            },
            LibraryLoader::{GetProcAddress, LoadLibraryW},
        },
        UI::{
            Shell::{
                IPreviewHandler, IPreviewHandlerVisuals, IThumbnailProvider,
                PropertiesSystem::{IInitializeWithFile, IInitializeWithStream},
                SHCreateStreamOnFileEx, WTS_ALPHATYPE,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, PeekMessageW,
                RegisterClassW, SetForegroundWindow, TranslateMessage, CW_USEDEFAULT, MSG,
                PM_REMOVE, WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
            },
        },
    },
};

const CLSID_PREVIEW: GUID = GUID::from_u128(0x5b9e1d2a_7c43_4f8e_9a61_3d2b8c7e4f10);
const CLSID_THUMBNAIL: GUID = GUID::from_u128(0x8e4a6c1f_2d57_4b39_b8e2_6f1a9d3c5e27);

type GetClassObject =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

unsafe fn factory(get: GetClassObject, clsid: &GUID) -> IClassFactory {
    let mut ptr = std::ptr::null_mut();
    get(clsid, &IClassFactory::IID, &mut ptr)
        .ok()
        .expect("DllGetClassObject");
    IClassFactory::from_raw(ptr)
}

unsafe fn bitmap_to_png(hbmp: HBITMAP, background: Option<[u8; 3]>) -> Vec<u8> {
    let mut bm = BITMAP::default();
    GetObjectW(
        hbmp.into(),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut _ as *mut c_void),
    );
    let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    let dc = GetDC(None);
    GetDIBits(
        dc,
        hbmp,
        0,
        h as u32,
        Some(pixels.as_mut_ptr() as *mut c_void),
        &mut info,
        DIB_RGB_COLORS,
    );
    ReleaseDC(None, dc);
    png::encode_bgra_premultiplied(w as u32, h as u32, &pixels, background)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn pump_for(d: Duration) {
    let end = Instant::now() + d;
    let mut msg = MSG::default();
    while Instant::now() < end {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn main() -> windows::core::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dll, out_dir, models @ ..] = &args[..] else {
        panic!("usage: shelltest <dll> <out-dir> <model>...");
    };
    let model = &models[0];
    let out_dir = PathBuf::from(out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();

    if std::env::var_os("BABYLON_SHELL_LOG").is_none() {
        std::env::set_var("BABYLON_SHELL_LOG", out_dir.join("shell.log"));
    }
    unsafe {
        // Physical pixels, so the screen capture below lines up with the window.
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
        // SHELLTEST_MTA=1 calls the handler from an MTA thread, as prevhost.exe does.
        let apartment = if std::env::var_os("SHELLTEST_MTA").is_some() {
            COINIT_MULTITHREADED
        } else {
            COINIT_APARTMENTTHREADED
        };
        CoInitializeEx(None, apartment).ok()?;
        // "--registered" goes through the registry like Explorer: the preview handler is
        // created out of process in prevhost.exe, the thumbnail provider in process.
        let registered = dll == "--registered";
        let create = |clsid: &GUID| -> windows::core::Result<windows::core::IUnknown> {
            if registered {
                let ctx = if *clsid == CLSID_PREVIEW {
                    CLSCTX_LOCAL_SERVER
                } else {
                    CLSCTX_INPROC_SERVER
                };
                CoCreateInstance(clsid, None, ctx)
            } else {
                let lib = LoadLibraryW(&HSTRING::from(dll.as_str()))?;
                let get: GetClassObject = std::mem::transmute(
                    GetProcAddress(lib, PCSTR(b"DllGetClassObject\0".as_ptr())).expect("export"),
                );
                factory(get, clsid).CreateInstance(None)
            }
        };

        // --- Thumbnail, exactly as the thumbnail cache calls it ---
        let thumb: IThumbnailProvider = create(&CLSID_THUMBNAIL)?.cast()?;
        let stream: IStream =
            SHCreateStreamOnFileEx(&HSTRING::from(model.as_str()), STGM_READ.0, 0, false, None)?;
        thumb
            .cast::<IInitializeWithStream>()?
            .Initialize(&stream, STGM_READ.0)?;
        let mut hbmp = HBITMAP::default();
        let mut alpha = WTS_ALPHATYPE::default();
        let started = Instant::now();
        thumb.GetThumbnail(256, &mut hbmp, &mut alpha)?;
        println!("thumbnail: {:?}, alpha type {}", started.elapsed(), alpha.0);
        std::fs::write(
            out_dir.join("thumbnail.png"),
            bitmap_to_png(hbmp, Some([255, 255, 255])),
        )
        .unwrap();
        let _ = DeleteObject(hbmp.into());

        // --- Preview handler hosted in a window, like prevhost.exe does ---
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            lpszClassName: w!("ShellTestFrame"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let frame = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("ShellTestFrame"),
            w!("Preview test"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            820,
            560,
            None,
            None,
            None,
            None,
        )?;
        let mut rect = RECT::default();
        GetClientRect(frame, &mut rect)?;

        // Preview each file in turn with a fresh handler in the same process, like Explorer
        // does when the selection moves from file to file.
        for (index, model) in models.iter().enumerate() {
            let preview: IPreviewHandler = create(&CLSID_PREVIEW)?.cast()?;
            println!(
                "preview handler created; supports stream init: {}",
                preview.cast::<IInitializeWithStream>().is_ok()
            );
            preview
                .cast::<IInitializeWithFile>()?
                .Initialize(&HSTRING::from(model.as_str()), STGM_READ.0)?;
            preview
                .cast::<IPreviewHandlerVisuals>()?
                .SetBackgroundColor(COLORREF(0x00202020))?;
            preview.SetWindow(frame, &rect)?;
            let started = Instant::now();
            preview.DoPreview()?;
            pump_for(Duration::from_secs(5));
            println!("preview {index} ({model}): pumped {:?}", started.elapsed());

            let (w, h) = (rect.right, rect.bottom);
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(dc, bmp.into());
            // Copy what is actually on screen (WebView2 draws via DirectComposition).
            let mut origin = POINT::default();
            let _ = ClientToScreen(frame, &mut origin);
            let _ = SetForegroundWindow(frame);
            pump_for(Duration::from_millis(300));
            let ok = BitBlt(dc, 0, 0, w, h, Some(screen), origin.x, origin.y, SRCCOPY).is_ok();
            SelectObject(dc, old);
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            println!("capture: {ok}");
            std::fs::write(
                out_dir.join(format!("preview-{index}.png")),
                bitmap_to_png(bmp, None),
            )
            .unwrap();
            preview.Unload()?;
            pump_for(Duration::from_millis(500));
        }
    }
    Ok(())
}
