pub mod codec;
pub mod transport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoResolution {
    P720,
    P1080,
}

impl VideoResolution {
    pub fn width(&self) -> u32 {
        match self {
            VideoResolution::P720 => 1280,
            VideoResolution::P1080 => 1920,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            VideoResolution::P720 => 720,
            VideoResolution::P1080 => 1080,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            VideoResolution::P720 => "720p",
            VideoResolution::P1080 => "1080p",
        }
    }

    pub fn all() -> &'static [VideoResolution] {
        &[VideoResolution::P720, VideoResolution::P1080]
    }
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
            resolution: VideoResolution::P720,
            framerate: 15,
            sharing_enabled: false,
        }
    }
}
