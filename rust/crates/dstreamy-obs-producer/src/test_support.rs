use crate::control::{Named, Started};
use crate::model::{Canvas, PanelModel, PanelSettings};
use crate::panel_config::ConfigState;

pub fn named(id: &str, name: &str) -> Named {
    Named {
        id: id.into(),
        name: name.into(),
    }
}

/// OBS's canvas at 720p30.
pub fn canvas() -> Canvas {
    Canvas {
        fps_num: 30,
        fps_den: 1,
        output_width: 1280,
        output_height: 720,
    }
}

pub fn state(settings: PanelSettings) -> ConfigState {
    let mut s = ConfigState::default();
    s.model = PanelModel::new(settings);
    s
}

/// Starts `s` and lets the service and output accept it, in channel "c".
pub fn go_live(s: &mut ConfigState) {
    s.model.start(canvas()).unwrap();
    s.model.claim(None, &[]);
    s.model.started(Ok(Started {
        ownership: None,
        channel: "c".into(),
    }));
}
