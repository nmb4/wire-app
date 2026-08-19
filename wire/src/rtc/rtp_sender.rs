use anyhow::{anyhow, Result};
use bytes::Bytes;
use iroh_roq::{
    rtp,
    rtp::{
        codecs::opus::OpusPayloader,
        packet::Packet as RtpPacket,
        packetizer::{new_packetizer, Packetizer},
        sequence::Sequencer,
    },
    SendFlow,
};
use tokio::sync::broadcast::error::RecvError;
use tracing::trace;

use super::{MediaFrame, MediaTrack, TRACK_END_PAYLOAD};
use crate::codec::Codec;

#[derive(Debug)]
pub(crate) struct RtpMediaTrackSender {
    pub(crate) track: MediaTrack,
    pub(crate) send_flow: SendFlow,
}

pub(crate) const MTU: usize = 1100;

pub(crate) const CLOCK_RATE: u32 = crate::audio::SAMPLE_RATE.0;

impl RtpMediaTrackSender {
    pub(crate) async fn run(mut self) -> Result<()> {
        let result = self.run_inner().await;
        let end_packet = RtpPacket {
            header: rtp::header::Header {
                payload_type: self.track.codec().rtp_payload_type(),
                marker: true,
                ..Default::default()
            },
            payload: Bytes::from_static(TRACK_END_PAYLOAD),
        };
        // Ordinary audio uses QUIC datagrams, but track termination must not
        // be lossy or the remote volume UI can remain active forever. Send the
        // single end marker over an iroh-roq reliable stream for this flow.
        let end_result = async {
            let mut stream = self.send_flow.new_send_stream().await?;
            stream.send_rtp(&end_packet).await
        }
        .await;
        // Prevent any later datagrams from being emitted on this flow after
        // the reliable marker. The receiver closes its matching flow as soon
        // as it processes the marker.
        self.send_flow.close();
        match result {
            Ok(()) => end_result,
            Err(error) => Err(error),
        }
    }

    async fn run_inner(&mut self) -> Result<()> {
        let ssrc = 0;
        let sequencer: Box<dyn Sequencer + Send + Sync> =
            Box::new(rtp::sequence::new_random_sequencer());
        let payloader = match self.track.codec() {
            Codec::Opus { .. } => Box::new(OpusPayloader),
        };
        let payload_type = self.track.codec().rtp_payload_type();
        let mut packetizer = new_packetizer(
            MTU,
            payload_type,
            ssrc,
            payloader,
            sequencer.clone(),
            CLOCK_RATE,
        );
        loop {
            let frame = match self.track.recv().await {
                Ok(frame) => frame,
                Err(RecvError::Lagged(n)) => {
                    // increase sequence number for frames skipped due to lagging
                    for _ in 0..n {
                        sequencer.next_sequence_number();
                    }
                    continue;
                }
                Err(RecvError::Closed) => {
                    break;
                }
            };
            let MediaFrame {
                payload,
                sample_count,
                skipped_frames,
                skipped_samples,
            } = frame;
            // increase sequence number for frames skipped at source
            if let Some(skipped_frames) = skipped_frames {
                for _ in 0..skipped_frames {
                    sequencer.next_sequence_number();
                }
            }
            // increase timestamp for skipped samples
            // TODO: should also do that for skipped frames?
            if let Some(skipped_samples) = skipped_samples {
                packetizer.skip_samples(skipped_samples);
            }

            let sample_count = sample_count
                .ok_or_else(|| anyhow!("received media track frame without sample count"))?;
            let packets = packetizer.packetize(&payload, sample_count)?;
            for packet in packets {
                trace!(
                    "send packet len {} seq {} ts {}",
                    packet.payload.len(),
                    packet.header.sequence_number,
                    packet.header.timestamp,
                );
                self.send_flow.send_rtp(&packet)?;
            }
        }
        Ok(())
    }
}
