//! Windows GDI capture scaled to the target resolution in one blit.
//! Avoids allocating and copying a full native-resolution framebuffer.

use anyhow::{bail, Result};
use scopeguard::guard;
use windows::Win32::{
    Foundation::GetLastError,
    Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits,
        GetWindowDC, ReleaseDC, SelectObject, SetStretchBltMode, StretchBlt, BITMAPINFO,
        BITMAPINFOHEADER, COLORONCOLOR, DIB_RGB_COLORS, HBITMAP, SRCCOPY,
    },
    UI::WindowsAndMessaging::GetDesktopWindow,
};

pub fn capture_monitor_scaled(
    x: i32,
    y: i32,
    src_w: i32,
    src_h: i32,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<u8>> {
    let dst_w = dst_w as i32;
    let dst_h = dst_h as i32;
    let buffer_size = (dst_w * dst_h * 4) as usize;
    let mut buffer = vec![0u8; buffer_size];

    unsafe {
        let hwnd = GetDesktopWindow();
        let hdc_desktop = guard(GetWindowDC(Some(hwnd)), |hdc| {
            let _ = ReleaseDC(Some(hwnd), hdc);
        });

        let hdc_mem = guard(CreateCompatibleDC(Some(*hdc_desktop)), |hdc| {
            let _ = DeleteDC(hdc);
        });

        let h_bitmap = guard(
            CreateCompatibleBitmap(*hdc_desktop, dst_w, dst_h),
            |hbmp: HBITMAP| {
                let _ = DeleteObject(hbmp.into());
            },
        );

        SelectObject(*hdc_mem, (*h_bitmap).into());
        SetStretchBltMode(*hdc_mem, COLORONCOLOR);

        let ok = StretchBlt(
            *hdc_mem,
            0,
            0,
            dst_w,
            dst_h,
            Some(*hdc_desktop),
            x,
            y,
            src_w,
            src_h,
            SRCCOPY,
        )
        .as_bool();
        if !ok {
            bail!("StretchBlt failed: {:?}", GetLastError());
        }

        let mut bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: dst_w,
                biHeight: -dst_h,
                biPlanes: 1,
                biBitCount: 32,
                biSizeImage: buffer_size as u32,
                ..Default::default()
            },
            ..Default::default()
        };

        if GetDIBits(
            *hdc_mem,
            *h_bitmap,
            0,
            dst_h as u32,
            Some(buffer.as_mut_ptr().cast()),
            &mut bitmap_info,
            DIB_RGB_COLORS,
        ) == 0
        {
            bail!("GetDIBits failed: {:?}", GetLastError());
        }
    }

    Ok(buffer)
}

fn bgra_to_rgba(buffer: &mut [u8]) {
    for px in buffer.chunks_exact_mut(4) {
        px.swap(0, 2);
        if px[3] == 0 {
            px[3] = 255;
        }
    }
}

pub fn primary_monitor_geometry() -> Result<(i32, i32, i32, i32)> {
    let monitors = xcap::Monitor::all().map_err(|e| anyhow::anyhow!("{e}"))?;
    let monitor = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or(monitors.first())
        .ok_or_else(|| anyhow::anyhow!("no monitors found"))?;
    let x = monitor.x().map_err(|e| anyhow::anyhow!("{e}"))?;
    let y = monitor.y().map_err(|e| anyhow::anyhow!("{e}"))?;
    let w = monitor.width().map_err(|e| anyhow::anyhow!("{e}"))? as i32;
    let h = monitor.height().map_err(|e| anyhow::anyhow!("{e}"))? as i32;
    Ok((x, y, w, h))
}