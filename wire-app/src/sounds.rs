use std::io::Cursor;

use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};

macro_rules! optional_sound {
    ($cfg:ident, $path:literal) => {{
        #[cfg($cfg)]
        {
            Some(include_bytes!($path).as_slice())
        }
        #[cfg(not($cfg))]
        {
            None
        }
    }};
}

#[derive(Clone, Copy)]
pub enum Sound {
    Whoosh1,
    Whoosh2,
    Button1,
    Button2,
    Success,
    Fail,
    Notification,
    IncomingRing,
}

impl Sound {
    fn bytes(self) -> Option<&'static [u8]> {
        match self {
            Sound::Whoosh1 => optional_sound!(wire_has_sound_whoosh_1, "../sound-kit/whoosh-1.wav"),
            Sound::Whoosh2 => optional_sound!(wire_has_sound_whoosh_2, "../sound-kit/whoosh-2.wav"),
            Sound::Button1 => optional_sound!(wire_has_sound_button_1, "../sound-kit/button-1.wav"),
            Sound::Button2 => optional_sound!(wire_has_sound_button_2, "../sound-kit/button-2.wav"),
            Sound::Success => {
                optional_sound!(wire_has_sound_success, "../sound-kit/success.wav")
            }
            Sound::Fail => optional_sound!(wire_has_sound_fail, "../sound-kit/fail.wav"),
            Sound::Notification => optional_sound!(
                wire_has_sound_notification_pop,
                "../sound-kit/notification-pop.wav"
            ),
            Sound::IncomingRing => optional_sound!(
                wire_has_sound_incoming_ring,
                "../sound-kit/atmostphere-2.wav"
            ),
        }
    }
}

const HAS_ANY_SOUND: bool = cfg!(any(
    wire_has_sound_whoosh_1,
    wire_has_sound_whoosh_2,
    wire_has_sound_button_1,
    wire_has_sound_button_2,
    wire_has_sound_success,
    wire_has_sound_fail,
    wire_has_sound_notification_pop,
    wire_has_sound_incoming_ring,
));

pub struct Sounds {
    _stream: OutputStream,
    handle: OutputStreamHandle,
    ringtone: Option<Sink>,
    volume: f32,
}

impl Sounds {
    pub fn try_new(volume: f32) -> Option<Self> {
        if !HAS_ANY_SOUND {
            return None;
        }
        let (stream, handle) = OutputStream::try_default().ok()?;
        Some(Self {
            _stream: stream,
            handle,
            ringtone: None,
            volume: normalize_volume(volume),
        })
    }

    pub fn play(&self, sound: Sound) {
        let Some(bytes) = sound.bytes() else {
            return;
        };
        let Ok(sink) = Sink::try_new(&self.handle) else {
            return;
        };
        let Ok(decoder) = Decoder::new(Cursor::new(bytes)) else {
            return;
        };
        sink.set_volume(self.volume);
        sink.append(decoder);
        sink.detach();
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = normalize_volume(volume);
        if let Some(ringtone) = &self.ringtone {
            ringtone.set_volume(self.volume);
        }
    }

    pub fn set_incoming_ring(&mut self, active: bool) {
        if active {
            if self.ringtone.is_some() {
                return;
            }
            let Ok(sink) = Sink::try_new(&self.handle) else {
                return;
            };
            let Some(bytes) = Sound::IncomingRing.bytes() else {
                return;
            };
            let Ok(decoder) = Decoder::new(Cursor::new(bytes)) else {
                return;
            };
            sink.set_volume(self.volume);
            sink.append(decoder.repeat_infinite());
            self.ringtone = Some(sink);
        } else if let Some(sink) = self.ringtone.take() {
            sink.stop();
        }
    }
}

fn normalize_volume(volume: f32) -> f32 {
    if volume.is_finite() {
        volume.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_sound_is_embedded_and_decodable() {
        let bytes = Sound::Notification
            .bytes()
            .expect("notification Pop sound should be embedded");
        assert!(Decoder::new(Cursor::new(bytes)).is_ok());
    }

    #[test]
    fn sound_volume_is_limited_to_the_supported_range() {
        assert_eq!(normalize_volume(-0.5), 0.0);
        assert_eq!(normalize_volume(0.4), 0.4);
        assert_eq!(normalize_volume(1.5), 1.0);
        assert_eq!(normalize_volume(f32::NAN), 1.0);
    }
}
