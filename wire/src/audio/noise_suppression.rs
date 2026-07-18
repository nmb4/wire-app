use nnnoiseless::DenoiseState;

const FRAME_SIZE: usize = DenoiseState::FRAME_SIZE;
const PCM_SCALE: f32 = 32_768.0;

/// Realtime RNNoise processing for one 10 ms, 48 kHz frame at a time.
///
/// `nnnoiseless` uses floating-point storage for signed 16-bit PCM values, while
/// the rest of Wire uses normalized `f32` PCM. Buffers are retained here so the
/// audio callback does not allocate after initialization.
pub(super) struct RnnoiseSuppressor {
    channels: usize,
    states: Vec<Box<DenoiseState<'static>>>,
    input: Vec<[f32; FRAME_SIZE]>,
    output: Vec<[f32; FRAME_SIZE]>,
}

impl RnnoiseSuppressor {
    pub(super) fn new(channels: usize) -> Self {
        assert!(channels > 0, "RNNoise requires at least one channel");
        Self {
            channels,
            states: (0..channels).map(|_| DenoiseState::new()).collect(),
            input: vec![[0.0; FRAME_SIZE]; channels],
            output: vec![[0.0; FRAME_SIZE]; channels],
        }
    }

    pub(super) fn process_interleaved(&mut self, frame: &mut [f32]) {
        assert_eq!(
            frame.len(),
            FRAME_SIZE * self.channels,
            "RNNoise requires one interleaved 10 ms frame"
        );

        for sample_index in 0..FRAME_SIZE {
            for channel in 0..self.channels {
                let normalized = frame[sample_index * self.channels + channel].clamp(-1.0, 1.0);
                self.input[channel][sample_index] =
                    (normalized * PCM_SCALE).clamp(i16::MIN as f32, i16::MAX as f32);
            }
        }

        for channel in 0..self.channels {
            self.states[channel].process_frame(&mut self.output[channel], &self.input[channel]);
        }

        for sample_index in 0..FRAME_SIZE {
            for channel in 0..self.channels {
                frame[sample_index * self.channels + channel] =
                    (self.output[channel][sample_index] / PCM_SCALE).clamp(-1.0, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processes_engine_stereo_frames_in_place() {
        let mut suppressor = RnnoiseSuppressor::new(2);
        let mut frame = vec![0.0; FRAME_SIZE * 2];

        suppressor.process_interleaved(&mut frame);

        assert!(frame.iter().all(|sample| sample.is_finite()));
        assert!(frame.iter().all(|sample| (-1.0..=1.0).contains(sample)));
    }

    #[test]
    #[should_panic(expected = "RNNoise requires one interleaved 10 ms frame")]
    fn rejects_frames_with_the_wrong_size() {
        let mut suppressor = RnnoiseSuppressor::new(2);
        suppressor.process_interleaved(&mut [0.0; FRAME_SIZE]);
    }
}
