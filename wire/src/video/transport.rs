use anyhow::{Context, Result};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const VIDEO_PROTOCOL_MAGIC: [u8; 2] = *b"WV";
const VIDEO_PROTOCOL_VERSION: u8 = 1;
const VIDEO_FLAG_KEYFRAME: u8 = 1;
const VIDEO_FRAME_HEADER_LEN: usize = 20;
const MAX_VIDEO_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedVideoFrame {
    pub sequence: u64,
    pub sent_at_micros: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

impl EncodedVideoFrame {
    pub fn new(sequence: u64, keyframe: bool, data: Vec<u8>) -> Self {
        Self {
            sequence,
            sent_at_micros: now_micros(),
            keyframe,
            data,
        }
    }
}

#[derive(Debug, Default)]
pub struct KeyframeGate {
    waiting: bool,
}

impl KeyframeGate {
    pub fn waiting() -> Self {
        Self { waiting: true }
    }

    pub fn require_keyframe(&mut self) {
        self.waiting = true;
    }

    pub fn is_waiting(&self) -> bool {
        self.waiting
    }

    pub fn accept(&mut self, frame: &EncodedVideoFrame) -> bool {
        if self.waiting && !frame.keyframe {
            return false;
        }
        self.waiting = false;
        true
    }
}

pub async fn send_frame<S: AsyncWrite + Unpin>(
    send: &mut S,
    frame: &EncodedVideoFrame,
) -> Result<()> {
    let payload_len = VIDEO_FRAME_HEADER_LEN
        .checked_add(frame.data.len())
        .context("video frame length overflow")?;
    if payload_len > MAX_VIDEO_FRAME_LEN {
        anyhow::bail!("video frame too large: {payload_len} bytes");
    }
    let len = payload_len as u32;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(&VIDEO_PROTOCOL_MAGIC).await?;
    send.write_u8(VIDEO_PROTOCOL_VERSION).await?;
    send.write_u8(if frame.keyframe {
        VIDEO_FLAG_KEYFRAME
    } else {
        0
    })
    .await?;
    send.write_all(&frame.sequence.to_be_bytes()).await?;
    send.write_all(&frame.sent_at_micros.to_be_bytes()).await?;
    send.write_all(&frame.data).await?;
    Ok(())
}

pub async fn recv_frame<R: AsyncRead + Unpin>(recv: &mut R) -> Result<Option<EncodedVideoFrame>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if !(VIDEO_FRAME_HEADER_LEN..=MAX_VIDEO_FRAME_LEN).contains(&len) {
        anyhow::bail!("invalid video frame length: {len} bytes");
    }
    let mut header = [0u8; VIDEO_FRAME_HEADER_LEN];
    recv.read_exact(&mut header).await?;
    if header[..2] != VIDEO_PROTOCOL_MAGIC {
        anyhow::bail!("invalid video protocol magic");
    }
    if header[2] != VIDEO_PROTOCOL_VERSION {
        anyhow::bail!("unsupported video protocol version: {}", header[2]);
    }
    if header[3] & !VIDEO_FLAG_KEYFRAME != 0 {
        anyhow::bail!("unsupported video frame flags: {:#04x}", header[3]);
    }
    let sequence = u64::from_be_bytes(header[4..12].try_into().unwrap());
    let sent_at_micros = u64::from_be_bytes(header[12..20].try_into().unwrap());
    let mut data = vec![0u8; len - VIDEO_FRAME_HEADER_LEN];
    recv.read_exact(&mut data).await?;
    Ok(Some(EncodedVideoFrame {
        sequence,
        sent_at_micros,
        keyframe: header[3] & VIDEO_FLAG_KEYFRAME != 0,
        data,
    }))
}

pub fn frame_age_ms(sent_at_micros: u64) -> Option<f64> {
    let now = now_micros();
    if sent_at_micros > now {
        return None;
    }
    Some((now - sent_at_micros) as f64 / 1000.0)
}

pub fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_round_trip_preserves_metadata() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let expected = EncodedVideoFrame {
            sequence: 42,
            sent_at_micros: 123_456,
            keyframe: true,
            data: vec![1, 2, 3, 4],
        };
        send_frame(&mut writer, &expected).await.unwrap();
        let actual = recv_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn rejects_invalid_header() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer
            .write_all(&(VIDEO_FRAME_HEADER_LEN as u32).to_be_bytes())
            .await
            .unwrap();
        writer
            .write_all(&[0; VIDEO_FRAME_HEADER_LEN])
            .await
            .unwrap();
        assert!(recv_frame(&mut reader).await.is_err());
    }

    #[test]
    fn keyframe_gate_recovers_after_sequence_discontinuity() {
        let mut gate = KeyframeGate::waiting();
        let p_frame = EncodedVideoFrame {
            sequence: 8,
            sent_at_micros: 0,
            keyframe: false,
            data: vec![],
        };
        let idr = EncodedVideoFrame {
            sequence: 9,
            keyframe: true,
            ..p_frame.clone()
        };
        assert!(!gate.accept(&p_frame));
        assert!(gate.accept(&idr));
        assert!(gate.accept(&p_frame));
        gate.require_keyframe();
        assert!(!gate.accept(&p_frame));
    }

    #[tokio::test]
    async fn rejects_out_of_bounds_lengths() {
        for len in [VIDEO_FRAME_HEADER_LEN - 1, MAX_VIDEO_FRAME_LEN + 1] {
            let (mut writer, mut reader) = tokio::io::duplex(16);
            writer.write_all(&(len as u32).to_be_bytes()).await.unwrap();
            assert!(recv_frame(&mut reader).await.is_err());
        }
    }
}
