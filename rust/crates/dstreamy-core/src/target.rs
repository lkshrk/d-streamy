use serde::{Deserialize, Serialize};

/// The window or display to capture.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureTarget {
    Display {
        id: u32,
    },
    Window {
        id: u32,
    },
    /// Chosen in the desktop portal dialog; the restore token skips the dialog next time.
    Portal {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        restore_token: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct W {
        target: CaptureTarget,
    }

    #[test]
    fn toml_round_trip() {
        for target in [
            CaptureTarget::Display { id: 1 },
            CaptureTarget::Window { id: 42 },
            CaptureTarget::Portal {
                restore_token: None,
            },
            CaptureTarget::Portal {
                restore_token: Some("abc".into()),
            },
        ] {
            let w = W { target };
            let s = toml::to_string(&w).unwrap();
            assert_eq!(toml::from_str::<W>(&s).unwrap(), w, "{s}");
        }
    }

    #[test]
    fn toml_shape() {
        let w: W = toml::from_str("[target]\nkind = \"window\"\nid = 7\n").unwrap();
        assert_eq!(w.target, CaptureTarget::Window { id: 7 });
    }
}
