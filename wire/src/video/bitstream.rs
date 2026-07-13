/// Normalize H.264 bitstreams for decoders that expect Annex B start codes.
pub fn normalize_h264_for_decode(data: &[u8]) -> Vec<u8> {
    if has_annex_b_start_code(data) {
        return data.to_vec();
    }
    avcc_to_annex_b(data).unwrap_or_else(|| data.to_vec())
}

/// Returns true when an H.264 access unit contains an IDR picture (NAL type 5).
/// Supports both Annex B and four-byte AVCC length-prefixed bitstreams.
pub fn contains_idr(data: &[u8]) -> bool {
    if has_annex_b_start_code(data) {
        return annex_b_nal_types(data).any(|nal_type| nal_type == 5);
    }

    let mut offset = 0usize;
    while offset + 4 <= data.len() {
        let len = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;
        if len == 0 || offset + len > data.len() {
            return false;
        }
        if data[offset] & 0x1f == 5 {
            return true;
        }
        offset += len;
    }
    false
}

fn annex_b_nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    (0..data.len()).filter_map(move |i| {
        let header = if data.get(i..i + 4) == Some(&[0, 0, 0, 1]) {
            i + 4
        } else if data.get(i..i + 3) == Some(&[0, 0, 1]) {
            i + 3
        } else {
            return None;
        };
        data.get(header).map(|byte| byte & 0x1f)
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_annex_b_idr() {
        assert!(contains_idr(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2]));
        assert!(!contains_idr(&[0, 0, 1, 0x41, 2, 3]));
    }

    #[test]
    fn detects_avcc_idr() {
        assert!(contains_idr(&[0, 0, 0, 2, 0x65, 1]));
        assert!(!contains_idr(&[0, 0, 0, 2, 0x41, 1]));
        assert!(!contains_idr(&[0, 0, 0, 8, 0x65]));
    }
}
