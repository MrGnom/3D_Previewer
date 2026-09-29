use std::{cell::RefCell, ffi::c_void, panic::AssertUnwindSafe, path::Path};

use windows::{
    core::{implement, Ref, Result},
    Win32::{
        Foundation::{E_FAIL, E_POINTER, E_UNEXPECTED},
        Graphics::Gdi::{
            CreateDIBSection, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
        },
        System::Com::{CoTaskMemFree, IStream, STATFLAG_DEFAULT, STATSTG},
        UI::Shell::{
            IThumbnailProvider, IThumbnailProvider_Impl,
            PropertiesSystem::{IInitializeWithStream, IInitializeWithStream_Impl},
            WTSAT_ARGB, WTS_ALPHATYPE,
        },
    },
};

use crate::{model, render, render_bytes};

/// Refuse to thumbnail absurdly large files rather than stall Explorer.
const MAX_BYTES: usize = 1 << 30;

struct Input {
    bytes: Vec<u8>,
    format: Option<model::Format>,
}

#[implement(IInitializeWithStream, IThumbnailProvider)]
pub struct ThumbnailProvider {
    input: RefCell<Option<Input>>,
}

impl ThumbnailProvider {
    pub fn new() -> Self {
        Self {
            input: RefCell::new(None),
        }
    }
}

fn stream_name(stream: &IStream) -> Option<String> {
    let mut stat = STATSTG::default();
    unsafe {
        stream.Stat(&mut stat, STATFLAG_DEFAULT).ok()?;
        if stat.pwcsName.is_null() {
            return None;
        }
        let name = stat.pwcsName.to_string().ok();
        CoTaskMemFree(Some(stat.pwcsName.0 as *const c_void));
        name
    }
}

fn read_all(stream: &IStream) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let mut read = 0u32;
        unsafe {
            stream.Read(
                chunk.as_mut_ptr() as *mut c_void,
                chunk.len() as u32,
                Some(&mut read),
            )
        }
        .ok()?;
        if read == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..read as usize]);
        if out.len() > MAX_BYTES {
            return Err(E_FAIL.into());
        }
    }
    Ok(out)
}

impl IInitializeWithStream_Impl for ThumbnailProvider_Impl {
    fn Initialize(&self, stream: Ref<IStream>, _mode: u32) -> Result<()> {
        let stream = stream.ok()?;
        let format = stream_name(stream).and_then(|n| {
            Path::new(&n)
                .extension()
                .and_then(|e| e.to_str())
                .and_then(model::Format::from_extension)
        });
        let bytes = read_all(stream)?;
        *self.input.borrow_mut() = Some(Input { bytes, format });
        Ok(())
    }
}

fn to_hbitmap(image: &render::Image) -> Result<HBITMAP> {
    let size = image.size as i32;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
    if bits.is_null() {
        return Err(E_FAIL.into());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(image.bgra.as_ptr(), bits as *mut u8, image.bgra.len())
    };
    Ok(bitmap)
}

impl IThumbnailProvider_Impl for ThumbnailProvider_Impl {
    fn GetThumbnail(
        &self,
        cx: u32,
        phbmp: *mut HBITMAP,
        pdwalpha: *mut WTS_ALPHATYPE,
    ) -> Result<()> {
        if phbmp.is_null() || pdwalpha.is_null() {
            return Err(E_POINTER.into());
        }
        let input = self.input.borrow();
        let input = input.as_ref().ok_or(E_UNEXPECTED)?;
        // Never let a malformed file unwind across the COM boundary into Explorer.
        let image = std::panic::catch_unwind(AssertUnwindSafe(|| {
            render_bytes(&input.bytes, input.format, cx)
        }))
        .ok()
        .flatten()
        .ok_or(E_FAIL)?;
        let bitmap = to_hbitmap(&image)?;
        unsafe {
            *phbmp = bitmap;
            *pdwalpha = WTSAT_ARGB;
        }
        Ok(())
    }
}
