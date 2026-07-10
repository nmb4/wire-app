use std::io::Cursor;

use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};

#[derive(Clone, Copy)]
pub enum Sound {
    Whoosh1,
    Whoosh2,
    Button1,
    Button2,
    Success,
    Fail,
    IncomingRing,
}

impl Sound {
    fn bytes(self) -> &'static [u8] {
        match self {
            Sound::Whoosh1 => include_bytes!("../sound-kit/whoosh-1.wav"),
            Sound::Whoosh2 => include_bytes!("../sound-kit/whoosh-2.wav"),
            Sound::Button1 => include_bytes!("../sound-kit/button-1.wav"),
            Sound::Button2 => include_bytes!("../sound-kit/button-2.wav"),
            Sound::Success => include_bytes!("../sound-kit/success.wav"),
            Sound::Fail => include_bytes!("../sound-kit/fail.wav"),
            Sound::IncomingRing => include_bytes!("../sound-kit/atmostphere-2.wav"),
        }
    }
}

pub struct Sounds {
    _stream: OutputStream,
    handle: OutputStreamHandle,
    ringtone: Option<Sink>,
}

impl Sounds {
    pub fn try_new() -> Option<Self> {
        let (stream, handle) = OutputStream::try_default().ok()?;
        Some(Self {
            _stream: stream,
            handle,
            ringtone: None,
        })
    }

    pub fn play(&self, sound: Sound) {
        let Ok(sink) = Sink::try_new(&self.handle) else {
            return;
        };
        let Ok(decoder) = Decoder::new(Cursor::new(sound.bytes())) else {
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
            let Ok(decoder) = Decoder::new(Cursor::new(Sound::IncomingRing.bytes())) else {
                return;
            };
            sink.append(decoder.repeat_infinite());
            self.ringtone = Some(sink);
        } else if let Some(sink) = self.ringtone.take() {
            sink.stop();
        }
    }
}