//! Dedicated decode thread. Uses OpenH264 to match the software encoder bitstream.

use std::sync::mpsc::{self, RecvTimeoutError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;
use callme::video::codec::VideoDecoder;
use tracing::{info, warn};

pub struct DecodedFrame {
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

struct FrameDecoder(VideoDecoder);

impl FrameDecoder {
    fn try_new() -> Result<Self> {
        info!("using OpenH264 software video decoder");
        Ok(Self(VideoDecoder::new()?))
    }

    fn decode(&mut self, data: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
        self.0.decode(data)
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
    let mut decode_errors = 0u64;

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
                decode_errors = 0;
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
                decode_errors += 1;
                if decode_errors <= 5 || decode_errors % 60 == 0 {
                    warn!("video decode error (#{decode_errors}): {e:?}");
                }
            }
        }
    }
    Ok(())
}