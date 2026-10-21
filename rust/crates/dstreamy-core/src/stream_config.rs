use std::fmt;
use std::ops::RangeInclusive;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::codec::Codec;
use crate::crop::CropRect;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum InvalidStreamConfig {
    #[error(
        "invalid resolution {0:?}: expected WIDTHxHEIGHT (even, at most 1920x1080) or 720p/900p/1080p"
    )]
    Resolution(String),
    #[error("invalid fps {0}: expected 15, 30 or 60")]
    Fps(u32),
    #[error("invalid bitrate {0} kbps: expected 1000..=15000")]
    Bitrate(u32),
    #[error("invalid audio gain {0}: expected 0.0..=3.0")]
    AudioGain(f32),
    #[error("invalid crop {0}: width and height must be positive")]
    EmptyCrop(CropRect),
    #[error("invalid crop {0}: its right and bottom edges must fit in u32")]
    CropOverflow(CropRect),
}

/// Maximum output size; the capture is scaled down to fit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Resolution {
    width: u32,
    height: u32,
}

impl Resolution {
    pub const P720: Self = Self {
        width: 1280,
        height: 720,
    };
    pub const P900: Self = Self {
        width: 1600,
        height: 900,
    };
    pub const P1080: Self = Self {
        width: 1920,
        height: 1080,
    };
    pub const MAX_WIDTH: u32 = 1920;
    pub const MAX_HEIGHT: u32 = 1080;

    pub fn new(width: u32, height: u32) -> Result<Self, InvalidStreamConfig> {
        let ok = |v: u32, max: u32| (2..=max).contains(&v) && v.is_multiple_of(2);
        if ok(width, Self::MAX_WIDTH) && ok(height, Self::MAX_HEIGHT) {
            Ok(Self { width, height })
        } else {
            Err(InvalidStreamConfig::Resolution(format!("{width}x{height}")))
        }
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn height(self) -> u32 {
        self.height
    }
}

impl Default for Resolution {
    fn default() -> Self {
        Self::P1080
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

impl FromStr for Resolution {
    type Err = InvalidStreamConfig;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidStreamConfig::Resolution(s.to_owned());
        match s {
            "720p" => Ok(Self::P720),
            "900p" => Ok(Self::P900),
            "1080p" => Ok(Self::P1080),
            _ => {
                let (w, h) = s.split_once('x').ok_or_else(invalid)?;
                let w = w.parse().map_err(|_| invalid())?;
                let h = h.parse().map_err(|_| invalid())?;
                Self::new(w, h).map_err(|_| invalid())
            }
        }
    }
}

impl TryFrom<String> for Resolution {
    type Error = InvalidStreamConfig;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Resolution> for String {
    fn from(r: Resolution) -> Self {
        r.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub enum Fps {
    Fps15,
    #[default]
    Fps30,
    Fps60,
}

impl Fps {
    pub const fn get(self) -> u32 {
        match self {
            Self::Fps15 => 15,
            Self::Fps30 => 30,
            Self::Fps60 => 60,
        }
    }
}

impl fmt::Display for Fps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

impl TryFrom<u32> for Fps {
    type Error = InvalidStreamConfig;

    fn try_from(v: u32) -> Result<Self, Self::Error> {
        match v {
            15 => Ok(Self::Fps15),
            30 => Ok(Self::Fps30),
            60 => Ok(Self::Fps60),
            other => Err(InvalidStreamConfig::Fps(other)),
        }
    }
}

impl From<Fps> for u32 {
    fn from(f: Fps) -> Self {
        f.get()
    }
}

/// Encoder and audio settings for a Session.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamConfig {
    pub resolution: Resolution,
    pub fps: Fps,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    pub audio_gain: f32,
}

impl StreamConfig {
    pub const BITRATE_KBPS: RangeInclusive<u32> = 1_000..=15_000;
    pub const AUDIO_GAIN: RangeInclusive<f32> = 0.0..=3.0;

    pub fn validate(&self) -> Result<(), InvalidStreamConfig> {
        if !Self::BITRATE_KBPS.contains(&self.bitrate_kbps) {
            return Err(InvalidStreamConfig::Bitrate(self.bitrate_kbps));
        }
        if !Self::AUDIO_GAIN.contains(&self.audio_gain) {
            return Err(InvalidStreamConfig::AudioGain(self.audio_gain));
        }
        Ok(())
    }
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            resolution: Resolution::default(),
            fps: Fps::default(),
            bitrate_kbps: 6_000,
            codec: Codec::default(),
            audio_gain: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_app() {
        let c = StreamConfig::default();
        assert_eq!(c.resolution, Resolution::P1080);
        assert_eq!(c.fps.get(), 30);
        assert_eq!(c.bitrate_kbps, 6_000);
        assert_eq!(c.codec, Codec::H264);
        assert_eq!(c.audio_gain, 1.0);
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn resolution_parses_presets_and_explicit_sizes() {
        assert_eq!("720p".parse(), Ok(Resolution::P720));
        assert_eq!("900p".parse(), Ok(Resolution::P900));
        assert_eq!("1080p".parse(), Ok(Resolution::P1080));
        let r: Resolution = "1366x768".parse().unwrap();
        assert_eq!((r.width(), r.height()), (1366, 768));
        assert_eq!(r.to_string(), "1366x768");
    }

    #[test]
    fn resolution_rejects_above_1080p_odd_and_malformed() {
        for s in [
            "2560x1440",
            "1920x1081",
            "1921x1080",
            "1281x720",
            "0x0",
            "1280",
            "axb",
            "4k",
        ] {
            assert!(s.parse::<Resolution>().is_err(), "{s}");
        }
    }

    #[test]
    fn fps_accepts_only_15_30_60() {
        assert_eq!(Fps::try_from(15), Ok(Fps::Fps15));
        assert_eq!(Fps::try_from(30), Ok(Fps::Fps30));
        assert_eq!(Fps::try_from(60), Ok(Fps::Fps60));
        assert_eq!(Fps::try_from(24), Err(InvalidStreamConfig::Fps(24)));
    }

    #[test]
    fn validate_checks_bitrate_and_gain_ranges() {
        let base = StreamConfig::default();
        for kbps in [1_000, 15_000] {
            let c = StreamConfig {
                bitrate_kbps: kbps,
                ..base
            };
            assert_eq!(c.validate(), Ok(()));
        }
        for kbps in [999, 15_001] {
            let c = StreamConfig {
                bitrate_kbps: kbps,
                ..base
            };
            assert_eq!(c.validate(), Err(InvalidStreamConfig::Bitrate(kbps)));
        }
        for gain in [0.0, 3.0] {
            let c = StreamConfig {
                audio_gain: gain,
                ..base
            };
            assert_eq!(c.validate(), Ok(()));
        }
        for gain in [-0.1, 3.1, f32::NAN] {
            let c = StreamConfig {
                audio_gain: gain,
                ..base
            };
            assert!(c.validate().is_err());
        }
    }

    #[test]
    fn toml_uses_human_readable_values() {
        let c = StreamConfig {
            resolution: Resolution::P720,
            fps: Fps::Fps60,
            codec: Codec::H265,
            ..StreamConfig::default()
        };
        let s = toml::to_string(&c).unwrap();
        assert!(s.contains("resolution = \"1280x720\""), "{s}");
        assert!(s.contains("fps = 60"), "{s}");
        assert!(s.contains("codec = \"H265\""), "{s}");
        assert_eq!(toml::from_str::<StreamConfig>(&s).unwrap(), c);
        assert!(toml::from_str::<StreamConfig>("fps = 24").is_err());
        assert!(toml::from_str::<StreamConfig>("resolution = \"4k\"").is_err());
    }
}
