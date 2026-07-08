pub mod bitstream;
pub mod codec;
pub mod transport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoResolution {
    P720,
    P1080,
    P1440,
}

impl VideoResolution {
    pub fn width(&self) -> u32 {
        match self {
            VideoResolution::P720 => 1280,
            VideoResolution::P1080 => 1920,
            VideoResolution::P1440 => 2560,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            VideoResolution::P720 => 720,
            VideoResolution::P1080 => 1080,
            VideoResolution::P1440 => 1440,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            VideoResolution::P720 => "720p",
            VideoResolution::P1080 => "1080p",
            VideoResolution::P1440 => "1440p",
        }
    }

    pub fn pixels(&self) -> u32 {
        self.width() * self.height()
    }

    pub fn all() -> &'static [VideoResolution] {
        &[VideoResolution::P720, VideoResolution::P1080, VideoResolution::P1440]
    }
}

/// Suggested bitrate in bits per second for screen content at the given resolution.
pub fn default_bitrate(resolution: VideoResolution, framerate: u32) -> u32 {
    let base = match resolution {
        VideoResolution::P720 => 5_000_000,
        VideoResolution::P1080 => 12_000_000,
        VideoResolution::P1440 => 20_000_000,
    };
    (base as f64 * (framerate as f64 / 30.0)) as u32
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoConfig {
    pub resolution: VideoResolution,
    pub framerate: u32,
    pub sharing_enabled: bool,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            resolution: VideoResolution::P1080,
            framerate: 30,
            sharing_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPreset {
    pub resolution: VideoResolution,
    pub framerate: u32,
    pub label: &'static str,
}

impl StreamPreset {
    pub fn all() -> &'static [StreamPreset] {
        &[
            StreamPreset {
                resolution: VideoResolution::P720,
                framerate: 30,
                label: "720p · 30 fps",
            },
            StreamPreset {
                resolution: VideoResolution::P1080,
                framerate: 30,
                label: "1080p · 30 fps",
            },
            StreamPreset {
                resolution: VideoResolution::P1080,
                framerate: 60,
                label: "1080p · 60 fps",
            },
            StreamPreset {
                resolution: VideoResolution::P1440,
                framerate: 30,
                label: "1440p · 30 fps",
            },
        ]
    }

    pub fn matches(config: &VideoConfig) -> Option<&'static StreamPreset> {
        Self::all()
            .iter()
            .find(|p| p.resolution == config.resolution && p.framerate == config.framerate)
    }
}
