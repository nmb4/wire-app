//! Dedicated decode thread with hardware acceleration on Windows.

use std::sync::mpsc::{self, RecvTimeoutError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;
use callme::video::codec::VideoDecoder;
use tracing::info;

#[cfg(windows)]
use crate::win_mf_codec::MfH264Decoder;

pub struct DecodedFrame {
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

enum FrameDecoder {
    #[cfg(windows)]
    MediaFoundation(MfH264Decoder),
    OpenH264(VideoDecoder),
}

impl FrameDecoder {
    fn try_new() -> Result<Self> {
        #[cfg(windows)]
        {
            match MfH264Decoder::try_new() {
                Ok(dec) => {
                    info!("using MF hardware video decoder");
                    return Ok(Self::MediaFoundation(dec));
                }
                Err(e) => info!("MF decoder unavailable, using OpenH264: {e:?}"),
            }
        }
        Ok(Self::OpenH264(VideoDecoder::new()?))
    }

    fn decode(&mut self, data: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
        match self {
            #[cfg(windows)]
            Self::MediaFoundation(dec) => dec.decode(data),
            Self::OpenH264(dec) => dec.decode(data),
        }
    }
}

const PACKET_QUEUE_DEPTH: usize = 2;

pub struct VideoDecodeWorker {
    packet_tx: mpsc::SyncSender<Vec<u8>>,
    join: Option<JoinHandle<()>>,
}

impl VideoDecodeWorker {
    pub fn spawn<F>(on_frame: F) -> Result<Self>
    where
        F: Fn(DecodedFrame) + Send + 'static,
    {
        let (packet_tx, packet_rx) = mpsc::sync_channel(PACKET_QUEUE_DEPTH);
        let join = thread::spawn(move || {
            if let Err(e) = run_decode_loop(packet_rx, on_frame) {
                info!("video decode thread stopped: {e:?}");
            }
        });
        Ok(Self {
            packet_tx,
            join: Some(join),
        })
    }

    pub fn submit(&self, data: Vec<u8>) {
        match self.packet_tx.try_send(data) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

impl Drop for VideoDecodeWorker {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run_decode_loop<F>(packet_rx: mpsc::Receiver<Vec<u8>>, on_frame: F) -> Result<()>
where
    F: Fn(DecodedFrame),
{
    let mut decoder = FrameDecoder::try_new()?;
    let mut decoded = 0u64;

    loop {
        let mut data = match packet_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(packet) => packet,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        while let Ok(newer) = packet_rx.try_recv() {
            data = newer;
        }

        match decoder.decode(&data) {
            Ok((rgba, w, h)) => {
                decoded += 1;
                if decoded == 1 {
                    info!("decoded first video frame ({w}x{h})");
                }
                on_frame(DecodedFrame {
                    data: Arc::new(rgba),
                    width: w,
                    height: h,
                });
            }
            Err(e) => {
                info!("video decode error: {e:?}");
            }
        }
    }
    Ok(())
}