use anyhow::Result;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn send_frame<S: AsyncWrite + Unpin>(
    send: &mut S,
    data: &[u8],
) -> Result<()> {
    let len = data.len() as u32;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(data).await?;
    Ok(())
}

pub async fn recv_frame<R: AsyncRead + Unpin>(
    recv: &mut R,
) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut data = vec![0u8; len];
    recv.read_exact(&mut data).await?;
    Ok(Some(data))
}
