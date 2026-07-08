/// Normalize H.264 bitstreams for decoders that expect Annex B start codes.
pub fn normalize_h264_for_decode(data: &[u8]) -> Vec<u8> {
    if has_annex_b_start_code(data) {
        return data.to_vec();
    }
    avcc_to_annex_b(data).unwrap_or_else(|| data.to_vec())
}

fn has_annex_b_start_code(data: &[u8]) -> bool {
    data.windows(4).any(|w| w == [0, 0, 0, 1]) || data.windows(3).any(|w| w == [0, 0, 1])
}

fn avcc_to_annex_b(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() + 32);
    let mut i = 0usize;
    while i + 4 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if len == 0 || i + len > data.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[i..i + len]);
        i += len;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}