/// Fast BGRA8 -> NV12 conversion for hardware encoders.
pub fn bgra_to_nv12(bgra: &[u8], width: u32, height: u32, out: &mut [u8]) {
    let w = width as usize;
    let h = height as usize;
    let y_size = w * h;
    let uv_size = w * h / 2;
    debug_assert_eq!(out.len(), y_size + uv_size);
    debug_assert_eq!(bgra.len(), w * h * 4);

    let (y_plane, uv_plane) = out.split_at_mut(y_size);

    for y in 0..h {
        let row = &bgra[y * w * 4..(y + 1) * w * 4];
        let y_row = &mut y_plane[y * w..(y + 1) * w];
        for (x, y_out) in y_row.iter_mut().enumerate() {
            let i = x * 4;
            let b = row[i] as i32;
            let g = row[i + 1] as i32;
            let r = row[i + 2] as i32;
            *y_out = ((66 * r + 129 * g + 25 * b + 128) >> 8).clamp(0, 255) as u8;
        }
    }

    for y in (0..h).step_by(2) {
        let row0 = &bgra[y * w * 4..(y + 1) * w * 4];
        let row1 = if y + 1 < h {
            &bgra[(y + 1) * w * 4..(y + 2) * w * 4]
        } else {
            row0
        };
        let uv_row = &mut uv_plane[(y / 2) * w..(y / 2 + 1) * w];
        for x in (0..w).step_by(2) {
            let i0 = x * 4;
            let i1 = i0 + 4;
            let (r, g, b) = avg_rgb(row0, i0, row1, i1);
            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8).clamp(0, 255) as u8;
            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8).clamp(0, 255) as u8;
            let uv_i = x;
            uv_row[uv_i] = u;
            uv_row[uv_i + 1] = v;
        }
    }
}

fn avg_rgb(row0: &[u8], i0: usize, row1: &[u8], i1: usize) -> (i32, i32, i32) {
    let r = (row0[i0 + 2] as i32 + row0[i1 + 2] as i32 + row1[i0 + 2] as i32 + row1[i1 + 2] as i32) / 4;
    let g = (row0[i0 + 1] as i32 + row0[i1 + 1] as i32 + row1[i0 + 1] as i32 + row1[i1 + 1] as i32) / 4;
    let b = (row0[i0] as i32 + row0[i1] as i32 + row1[i0] as i32 + row1[i1] as i32) / 4;
    (r, g, b)
}

/// NV12 -> RGBA8 for display. Processes 2x2 blocks to reduce UV fetches.
pub fn nv12_to_rgba(nv12: &[u8], width: u32, height: u32, out: &mut [u8]) {
    let w = width as usize;
    let h = height as usize;
    let y_size = w * h;
    debug_assert!(nv12.len() >= y_size + w * h / 2);
    debug_assert_eq!(out.len(), w * h * 4);

    let (y_plane, uv_plane) = nv12.split_at(y_size);

    for y in (0..h).step_by(2) {
        let uv_row = &uv_plane[(y / 2) * w..];
        for x in (0..w).step_by(2) {
            let u = uv_row[x] as i32 - 128;
            let v = uv_row[x + 1] as i32 - 128;
            let rv = (1436 * v) >> 10;
            let gu = (352 * u) >> 10;
            let gv = (731 * v) >> 10;
            let bu = (1814 * u) >> 10;

            for dy in 0..2 {
                let py = y + dy;
                if py >= h {
                    continue;
                }
                let y_row = &y_plane[py * w..(py + 1) * w];
                let out_row = &mut out[py * w * 4..(py + 1) * w * 4];
                for dx in 0..2 {
                    let px = x + dx;
                    if px >= w {
                        continue;
                    }
                    let y_val = y_row[px] as i32;
                    let i = px * 4;
                    out_row[i] = (y_val + rv).clamp(0, 255) as u8;
                    out_row[i + 1] = (y_val - gu - gv).clamp(0, 255) as u8;
                    out_row[i + 2] = (y_val + bu).clamp(0, 255) as u8;
                    out_row[i + 3] = 255;
                }
            }
        }
    }
}