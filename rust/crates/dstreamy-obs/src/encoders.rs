//! The encoders a start uses: D-Streamy's own Opus always, its own H.264 unless it shares a stream encoder that passes.

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::{Mutex, MutexGuard};

use dstreamy_obs_producer::settings::{self, EncoderSettings, Family, Problem};
use dstreamy_obs_sys as sys;

use crate::guard::guard;
use crate::log;
use crate::output::{OutputPtr, text};

/// obs-x264's own bitrate default (obs-x264.c:102); our choice for D-Streamy's encoder too.
const DEFAULT_BITRATE_KBPS: i64 = 6000;
const DEFAULT_VIDEO_ENCODER: &CStr = c"obs_x264";
const AUDIO_ENCODER: &CStr = c"ffmpeg_opus";

/// A reference to a libobs encoder that this plugin holds and releases.
pub struct EncoderRef(*mut sys::obs_encoder_t);

// SAFETY: libobs encoders are reference-counted objects; we only create, configure and release them on OBS threads.
unsafe impl Send for EncoderRef {}

impl EncoderRef {
    fn get(&self) -> *mut sys::obs_encoder_t {
        self.0
    }
}

impl Drop for EncoderRef {
    fn drop(&mut self) {
        // SAFETY: we hold one reference from obs_*_encoder_create; the output holds its own (obs-output.c:1024-1040).
        unsafe { sys::obs_encoder_release(self.0) };
    }
}

#[derive(Default)]
pub struct Owned {
    audio: Option<EncoderRef>,
    video: Option<EncoderRef>,
    /// The last stream encoder attached in share mode, kept so a start after a fallback checks it again.
    share: Option<EncoderRef>,
}

struct Config {
    share: bool,
    video_encoder: String,
    bitrate_kbps: i64,
    fallback: bool,
}

impl Config {
    fn read(output: OutputPtr) -> Self {
        // SAFETY: obs_output_get_settings adds a reference, released below; get_defaults filled the defaults.
        unsafe {
            let s = sys::obs_output_get_settings(output.0);
            let config = Self {
                share: text(sys::obs_data_get_string(s, c"mode".as_ptr())) == "share",
                video_encoder: text(sys::obs_data_get_string(s, c"video_encoder".as_ptr())),
                bitrate_kbps: sys::obs_data_get_int(s, c"bitrate".as_ptr()),
                fallback: sys::obs_data_get_bool(s, c"share_fallback".as_ptr()),
            };
            sys::obs_data_release(s);
            config
        }
    }
}

/// # Safety
/// `encoder` is a live encoder.
unsafe fn encoder_settings(encoder: *mut sys::obs_encoder_t) -> Result<EncoderSettings, String> {
    // SAFETY: the caller's contract; obs_encoder_get_settings adds a reference, released below.
    unsafe {
        let data = sys::obs_encoder_get_settings(encoder);
        let id = text(sys::obs_encoder_get_id(encoder));
        let codec = text(sys::obs_encoder_get_codec(encoder));
        let parsed = EncoderSettings::from_json(
            &id,
            &codec,
            &text(sys::obs_data_get_json_with_defaults(data)),
            &text(sys::obs_data_get_json(data)),
        );
        sys::obs_data_release(data);
        parsed
    }
}

/// # Safety
/// `encoder` is a live video encoder with its video set.
unsafe fn shape_problems(encoder: *mut sys::obs_encoder_t) -> Vec<Problem> {
    // SAFETY: the caller's contract; the info belongs to the encoder's video.
    unsafe {
        let (width, height) = (
            sys::obs_encoder_get_width(encoder),
            sys::obs_encoder_get_height(encoder),
        );
        match sys::video_output_get_info(sys::obs_encoder_video(encoder)).as_ref() {
            Some(info) => settings::check_shape(width, height, info.fps_num, info.fps_den),
            None => settings::check_shape(width, height, 0, 0),
        }
    }
}

/// Share only an encoder the stream is running: an idle one would be started and configured by us.
fn share_problems(candidate: *mut sys::obs_encoder_t) -> Result<Vec<Problem>, String> {
    // SAFETY: `candidate` is a live encoder we hold a reference to.
    unsafe {
        if !sys::obs_encoder_active(candidate) {
            return Ok(vec![Problem::StreamNotRunning]);
        }
        let mut problems = settings::check_video_encoder(&encoder_settings(candidate)?);
        problems.extend(shape_problems(candidate));
        Ok(problems)
    }
}

fn messages(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(Problem::message)
        .collect::<Vec<_>>()
        .join("; ")
}

/// libobs creates a placeholder for an unknown id and logs an error (obs-encoder.c:114-130), so ask first.
fn registered(id: &CStr) -> bool {
    // SAFETY: a NUL-terminated id; libobs only looks it up.
    !unsafe { sys::obs_get_encoder_codec(id.as_ptr()) }.is_null()
}

fn create_audio() -> Result<EncoderRef, String> {
    if !registered(AUDIO_ENCODER) {
        return Err(
            "D-Streamy needs the FFmpeg Opus audio encoder (ffmpeg_opus, from the obs-ffmpeg module)".into(),
        );
    }
    // SAFETY: a NULL settings object asks for the encoder's defaults; the audio output lives as long as libobs.
    unsafe {
        let encoder = sys::obs_audio_encoder_create(
            AUDIO_ENCODER.as_ptr(),
            c"D-Streamy audio".as_ptr(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
        );
        if encoder.is_null() {
            return Err(
                "D-Streamy needs the FFmpeg Opus audio encoder (ffmpeg_opus, from the obs-ffmpeg module)".into(),
            );
        }
        sys::obs_encoder_set_audio(encoder, sys::obs_get_audio());
        Ok(EncoderRef(encoder))
    }
}

fn create_video(config: &Config) -> Result<EncoderRef, String> {
    let family = Family::of(&config.video_encoder).ok_or_else(|| {
        Problem::UnknownEncoder {
            id: config.video_encoder.clone(),
        }
        .message()
    })?;
    let mut ovi = sys::obs_video_info::default();
    // SAFETY: `ovi` is a valid out parameter.
    if !unsafe { sys::obs_get_video_info(&mut ovi) } {
        return Err("D-Streamy found no video to encode".into());
    }
    let (fps, divisor) = settings::target_fps(ovi.fps_num, ovi.fps_den).map_err(|p| p.message())?;
    let (width, height) = settings::fit_1080p(ovi.output_width, ovi.output_height);
    let id = CString::new(config.video_encoder.as_str()).map_err(|e| e.to_string())?;
    // SAFETY: a NUL-terminated id; libobs only looks it up.
    let codec = unsafe { text(sys::obs_get_encoder_codec(id.as_ptr())) };
    if registered(&id) && codec != "h264" {
        return Err(Problem::NotH264 { codec }.message());
    }
    if !registered(&id) {
        return Err(format!(
            "D-Streamy's video encoder {} isn't available in this OBS: choose another in its settings",
            config.video_encoder
        ));
    }
    let json = CString::new(settings::own_video_settings(family, config.bitrate_kbps).to_string())
        .expect("JSON has no NUL");
    // SAFETY: valid strings; create applies the settings to its own object, so ours is released.
    let encoder = unsafe {
        let data = sys::obs_data_create_from_json(json.as_ptr());
        let encoder = sys::obs_video_encoder_create(
            id.as_ptr(),
            c"D-Streamy video".as_ptr(),
            data,
            ptr::null_mut(),
        );
        sys::obs_data_release(data);
        encoder
    };
    if encoder.is_null() {
        return Err(format!(
            "D-Streamy can't create the video encoder {}",
            config.video_encoder
        ));
    }
    let encoder = EncoderRef(encoder);
    // SAFETY: a new, inactive encoder; init-only settings are allowed before it initializes (obs-encoder.c:995-1040).
    unsafe {
        if (width, height) != (ovi.output_width, ovi.output_height) {
            sys::obs_encoder_set_scaled_size(encoder.get(), width, height);
            sys::obs_encoder_set_gpu_scale_type(encoder.get(), sys::OBS_SCALE_BICUBIC);
        }
        if divisor > 1 && !sys::obs_encoder_set_frame_rate_divisor(encoder.get(), divisor) {
            return Err(format!(
                "D-Streamy can't divide the frame rate down to {fps} FPS"
            ));
        }
        sys::obs_encoder_set_video(encoder.get(), sys::obs_get_video());
        let mut problems = settings::check_video_encoder(&encoder_settings(encoder.get())?);
        problems.extend(shape_problems(encoder.get()));
        if !problems.is_empty() {
            return Err(format!(
                "D-Streamy plugin error (its own encoder fails its checks: {})",
                messages(&problems)
            ));
        }
    }
    log::info(&format!(
        "own video encoder {} at {width}×{height}, {fps} FPS, {} kbps",
        config.video_encoder, config.bitrate_kbps
    ));
    Ok(encoder)
}

fn lock(owned: &Mutex<Owned>) -> MutexGuard<'_, Owned> {
    owned.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sets this start's encoders on the output, which is inactive; runs on the thread that called start.
pub fn prepare(output: OutputPtr, owned: &Mutex<Owned>) -> Result<(), String> {
    let config = Config::read(output);
    let mut owned = lock(owned);
    if owned.audio.is_none() {
        owned.audio = Some(create_audio()?);
    }
    let audio = owned
        .audio
        .as_ref()
        .map(EncoderRef::get)
        .expect("just created");
    // SAFETY: the output is inactive in start (obs-output.c:1024-1040 refuses otherwise) and the encoders are live.
    unsafe {
        let attached = sys::obs_output_get_audio_encoder(output.0, 0);
        if !attached.is_null() && attached != audio {
            log::info(&format!(
                "replacing the attached audio encoder {} with D-Streamy's own Opus encoder",
                text(sys::obs_encoder_get_id(attached))
            ));
        }
        sys::obs_output_set_audio_encoder(output.0, audio, 0);
    }
    // SAFETY: as above.
    let attached = unsafe { sys::obs_output_get_video_encoder(output.0) };
    let ours = owned.video.as_ref().map(EncoderRef::get);
    let shared = owned.share.as_ref().map(EncoderRef::get);
    if !attached.is_null() && Some(attached) != ours && Some(attached) != shared {
        // SAFETY: `attached` is the output's live video encoder; get_ref adds our own reference.
        let candidate = unsafe { sys::obs_encoder_get_ref(attached) };
        owned.share = (!candidate.is_null()).then_some(EncoderRef(candidate));
    }
    if config.share {
        match owned.share.as_ref().map(EncoderRef::get) {
            None => log::info("share mode: no stream encoder to share, so D-Streamy uses its own"),
            Some(candidate) => {
                let problems = share_problems(candidate)?;
                // SAFETY: `candidate` is live while we hold its reference.
                let id = unsafe { text(sys::obs_encoder_get_id(candidate)) };
                if problems.is_empty() {
                    // SAFETY: the output is inactive; it takes its own reference.
                    unsafe { sys::obs_output_set_video_encoder(output.0, candidate) };
                    log::info(&format!("sharing the stream's video encoder {id}"));
                    return Ok(());
                }
                let message = messages(&problems);
                if !config.fallback {
                    return Err(message);
                }
                log::warn(&format!(
                    "not sharing the stream's video encoder {id} ({message}), so D-Streamy uses its own"
                ));
            }
        }
    }
    let video = create_video(&config)?;
    // SAFETY: the output is inactive; it takes its own reference to the encoder.
    unsafe { sys::obs_output_set_video_encoder(output.0, video.get()) };
    owned.video = Some(video);
    Ok(())
}

pub fn check_audio(aenc: *mut sys::obs_encoder_t) -> Result<(), String> {
    // SAFETY: the output's initialized audio encoder.
    let problems = unsafe {
        settings::check_audio(
            &text(sys::obs_encoder_get_codec(aenc)),
            sys::obs_encoder_get_sample_rate(aenc),
            u32::try_from(sys::audio_output_get_channels(sys::obs_encoder_audio(aenc)))
                .unwrap_or(0),
            sys::obs_encoder_get_frame_size(aenc),
        )
    };
    if problems.is_empty() {
        Ok(())
    } else {
        Err(messages(&problems))
    }
}

pub extern "C" fn get_defaults(settings: *mut sys::obs_data_t) {
    guard("get_defaults", (), || {
        // SAFETY: libobs passes the settings object to fill.
        unsafe {
            sys::obs_data_set_default_string(settings, c"mode".as_ptr(), c"own".as_ptr());
            sys::obs_data_set_default_string(
                settings,
                c"video_encoder".as_ptr(),
                DEFAULT_VIDEO_ENCODER.as_ptr(),
            );
            sys::obs_data_set_default_int(settings, c"bitrate".as_ptr(), DEFAULT_BITRATE_KBPS);
            sys::obs_data_set_default_bool(settings, c"share_fallback".as_ptr(), true);
        }
    });
}

pub extern "C" fn get_properties(_data: *mut c_void) -> *mut sys::obs_properties_t {
    guard("get_properties", ptr::null_mut(), || {
        // SAFETY: libobs owns the returned properties; the strings are copied.
        unsafe {
            let props = sys::obs_properties_create();
            let mode = sys::obs_properties_add_list(
                props,
                c"mode".as_ptr(),
                c"Video encoder".as_ptr(),
                sys::OBS_COMBO_TYPE_LIST,
                sys::OBS_COMBO_FORMAT_STRING,
            );
            sys::obs_property_list_add_string(
                mode,
                c"D-Streamy's own encoder".as_ptr(),
                c"own".as_ptr(),
            );
            sys::obs_property_list_add_string(
                mode,
                c"Share the stream's encoder".as_ptr(),
                c"share".as_ptr(),
            );
            let encoders = sys::obs_properties_add_list(
                props,
                c"video_encoder".as_ptr(),
                c"Own encoder".as_ptr(),
                sys::OBS_COMBO_TYPE_LIST,
                sys::OBS_COMBO_FORMAT_STRING,
            );
            let mut id: *const c_char = ptr::null();
            let mut i = 0;
            while sys::obs_enum_encoder_types(i, &mut id) {
                i += 1;
                let deprecated =
                    sys::obs_get_encoder_caps(id) & sys::OBS_ENCODER_CAP_DEPRECATED != 0;
                if !deprecated
                    && sys::obs_get_encoder_type(id) == sys::OBS_ENCODER_VIDEO
                    && text(sys::obs_get_encoder_codec(id)) == "h264"
                    && Family::of(&text(id)).is_some()
                {
                    sys::obs_property_list_add_string(
                        encoders,
                        sys::obs_encoder_get_display_name(id),
                        id,
                    );
                }
            }
            sys::obs_properties_add_int(
                props,
                c"bitrate".as_ptr(),
                c"Bitrate (kbps)".as_ptr(),
                500,
                50_000,
                50,
            );
            sys::obs_properties_add_bool(
                props,
                c"share_fallback".as_ptr(),
                c"Use the own encoder when the stream's can't be shared".as_ptr(),
            );
            props
        }
    })
}
