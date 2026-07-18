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
}

impl Sounds {
    pub fn try_new() -> Option<Self> {
        if !HAS_ANY_SOUND {
            return None;
        }
        let (stream, handle) = OutputStream::try_default().ok()?;
        Some(Self {
            _stream: stream,
            handle,
            ringtone: None,
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
        sink.append(decoder);
        sink.detach();
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
            sink.append(decoder.repeat_infinite());
            self.ringtone = Some(sink);
        } else if let Some(sink) = self.ringtone.take() {
            sink.stop();
        }
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
}
