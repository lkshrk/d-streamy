use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Codec {
    #[default]
    H264,
    H265,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown codec {0:?}, expected \"H264\" or \"H265\"")]
pub struct UnknownCodec(pub String);

impl Codec {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::H265 => "H265",
        }
    }

    /// Parameter sets prepended on keyframes: SPS+PPS for H.264, VPS+SPS+PPS for H.265.
    pub const fn parameter_set_count(self) -> usize {
        match self {
            Self::H264 => 2,
            Self::H265 => 3,
        }
    }

    pub const fn viewer_warning(self) -> Option<&'static str> {
        match self {
            Self::H264 => None,
            Self::H265 => {
                Some("H.265 is experimental: most Discord viewers can't decode it and see no video")
            }
        }
    }

    /// Lenient parse: anything but an exact `"H265"` falls back to H.264.
    pub fn normalize(raw: Option<&str>) -> Self {
        match raw {
            Some("H265") => Self::H265,
            _ => Self::H264,
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Codec {
    type Err = UnknownCodec;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "H264" => Ok(Self::H264),
            "H265" => Ok(Self::H265),
            other => Err(UnknownCodec(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_h265_carries_a_viewer_warning() {
        assert_eq!(Codec::H264.viewer_warning(), None);
        assert!(Codec::H265.viewer_warning().is_some());
    }

    #[test]
    fn raw_values_match_ipc_strings() {
        assert_eq!(Codec::H264.as_str(), "H264");
        assert_eq!(Codec::H265.as_str(), "H265");
        assert_eq!("H265".parse::<Codec>(), Ok(Codec::H265));
        assert!("VP9".parse::<Codec>().is_err());
    }

    #[test]
    fn parameter_set_count() {
        assert_eq!(Codec::H264.parameter_set_count(), 2);
        assert_eq!(Codec::H265.parameter_set_count(), 3);
    }

    #[test]
    fn normalize_passes_h265_through() {
        assert_eq!(Codec::normalize(Some("H265")), Codec::H265);
    }

    #[test]
    fn normalize_passes_h264_through() {
        assert_eq!(Codec::normalize(Some("H264")), Codec::H264);
    }

    #[test]
    fn normalize_defaults_unknown_values_to_h264() {
        assert_eq!(Codec::normalize(Some("VP9")), Codec::H264);
        assert_eq!(Codec::normalize(Some("h265")), Codec::H264);
        assert_eq!(Codec::normalize(None), Codec::H264);
        assert_eq!(Codec::normalize(Some("")), Codec::H264);
        assert_eq!(Codec::normalize(Some("123")), Codec::H264);
    }

    #[test]
    fn serde_uses_ipc_strings() {
        #[derive(Serialize, Deserialize)]
        struct W {
            codec: Codec,
        }
        let s = toml::to_string(&W { codec: Codec::H265 }).unwrap();
        assert_eq!(s.trim(), "codec = \"H265\"");
        assert!(toml::from_str::<W>("codec = \"h265\"").is_err());
    }
}
