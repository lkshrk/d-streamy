use std::sync::{Arc, Mutex};

use dstreamy_media::CaptureConfig;
use gstreamer as gst;
use gstreamer::prelude::*;

use super::AudioInput;
use super::audio_branch::AudioOptions;
use super::elements::make;

pub(super) fn init() {
    gst::init().unwrap();
}

pub(super) fn config(width: u32, height: u32) -> CaptureConfig {
    CaptureConfig {
        width,
        height,
        fps: 30,
        bitrate_bps: 1_000_000,
        audio: false,
        ..CaptureConfig::default()
    }
}

/// Posts an error on `pipeline`'s bus as if from an element named `name`.
pub(super) fn post_error_as<T: gst::MessageErrorDomain>(
    pipeline: &gst::Pipeline,
    name: &str,
    code: T,
) {
    let fake = make("fakesrc").unwrap();
    fake.set_property("name", name);
    let msg = gst::message::Error::builder(code, "injected")
        .src(&fake)
        .build();
    pipeline.bus().unwrap().post(msg).unwrap();
}

pub(super) const REAL_INPUTS: [(AudioInput, &str); 2] = [
    (AudioInput::PulseMonitor, "pulsesrc"),
    (AudioInput::PipeWireSink, "pipewiresrc"),
];

pub(super) fn fake_device(current: &Arc<Mutex<String>>) -> AudioOptions {
    let current = current.clone();
    AudioOptions {
        pin_properties: true,
        device_query: Some(Arc::new(move |_| Some(current.lock().unwrap().clone()))),
    }
}

glib::wrapper! {
    pub struct BlockingSrc(ObjectSubclass<blocking::BlockingSrc>) @extends gst::Element, gst::Object;
}

impl BlockingSrc {
    pub(super) fn was_flushed(&self) -> bool {
        use glib::subclass::types::ObjectSubclassIsExt;
        *self.imp().released.0.lock().unwrap()
    }
}

/// Stands in for pipewiresrc's start wait: READY to PAUSED blocks until the source pad is flushed.
mod blocking {
    use std::sync::{Condvar, LazyLock, Mutex};
    use std::time::Duration;

    use gst::subclass::prelude::*;
    use gstreamer as gst;
    use gstreamer::prelude::*;

    pub struct BlockingSrc {
        pad: gst::Pad,
        pub(super) released: (Mutex<bool>, Condvar),
        pub(crate) fail_stop: std::sync::atomic::AtomicBool,
    }

    impl BlockingSrc {
        fn release(&self) {
            *self.released.0.lock().unwrap() = true;
            self.released.1.notify_all();
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BlockingSrc {
        const NAME: &'static str = "DstreamyTestBlockingSrc";
        type Type = super::BlockingSrc;
        type ParentType = gst::Element;

        fn with_class(klass: &Self::Class) -> Self {
            let templ = klass.pad_template("src").unwrap();
            let pad = gst::Pad::builder_from_template(&templ)
                .event_function(|_pad, parent, event| {
                    BlockingSrc::catch_panic_pad_function(
                        parent,
                        || false,
                        |this| {
                            if event.type_() == gst::EventType::FlushStart {
                                this.release();
                            }
                            true
                        },
                    )
                })
                .build();
            Self {
                pad,
                released: (Mutex::new(false), Condvar::new()),
                fail_stop: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    impl ObjectImpl for BlockingSrc {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().add_pad(&self.pad).unwrap();
        }
    }

    impl GstObjectImpl for BlockingSrc {}

    impl ElementImpl for BlockingSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                gst::subclass::ElementMetadata::new(
                    "Blocking source",
                    "Source/Audio",
                    "Blocks in READY to PAUSED until flushed",
                    "d-streamy tests",
                )
            });
            Some(&METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                vec![
                    gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &gst::Caps::new_any(),
                    )
                    .unwrap(),
                ]
            });
            TEMPLATES.as_ref()
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            if transition == gst::StateChange::ReadyToNull
                && self.fail_stop.load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(gst::StateChangeError);
            }
            let changed = self.parent_change_state(transition)?;
            // As pipewiresrc: the wait comes after the parent activated the pads, so a flush reaches it.
            if transition == gst::StateChange::ReadyToPaused {
                let (lock, flushed) = &self.released;
                let (released, _) = flushed
                    .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(30), |r| !*r)
                    .unwrap();
                if *released {
                    return Err(gst::StateChangeError);
                }
            }
            Ok(changed)
        }
    }
}
