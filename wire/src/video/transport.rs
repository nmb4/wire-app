use anyhow::Result;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const VIDEO_FRAME_HEADER_LEN: usize = 8;

pub async fn send_frame<S: AsyncWrite + Unpin>(send: &mut S, data: &[u8]) -> Result<()> {
    let len = (VIDEO_FRAME_HEADER_LEN + data.len()) as u32;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(&now_micros().to_be_bytes()).await?;
    send.write_all(data).await?;
    Ok(())
}

pub async fn recv_frame<R: AsyncRead + Unpin>(recv: &mut R) -> Result<Option<(Vec<u8>, u64)>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len < VIDEO_FRAME_HEADER_LEN {
        anyhow::bail!("video frame too short: {len} bytes");
    }
    let mut timestamp_buf = [0u8; VIDEO_FRAME_HEADER_LEN];
    recv.read_exact(&mut timestamp_buf).await?;
    let sent_at_micros = u64::from_be_bytes(timestamp_buf);
    let mut data = vec![0u8; len - VIDEO_FRAME_HEADER_LEN];
    recv.read_exact(&mut data).await?;
    Ok(Some((data, sent_at_micros)))
}

pub fn frame_age_ms(sent_at_micros: u64) -> Option<f64> {
    let now = now_micros();
    if sent_at_micros > now {
        return None;
    }
    Some((now - sent_at_micros) as f64 / 1000.0)
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u128::from(u64::MAX)) as u64
}
