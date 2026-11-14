//! The encoders a start uses: D-Streamy's own Opus always, its own H.264 unless it shares a stream encoder that passes.

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::Mutex;

use dstreamy_obs_producer::settings::{self, EncoderSettings, Family, Problem};
use dstreamy_obs_sys as sys;

use crate::guard::{guard, lock};
use crate::log;
use crate::output::{OutputPtr, text};

/// obs-x264's own bitrate default (obs-x264.c:102); our choice for D-Streamy's encoder too.
const DEFAULT_BITRATE_KBPS: i64 = 6000;
const DEFAULT_VIDEO_ENCODER: &CStr = c"obs_x264";
const AUDIO_ENCODER: &CStr = c"ffmpeg_opus";

/// A reference to a libobs encoder that this plugin holds and releases.
pub struct EncoderRef(*mut sys::obs_encoder_t);

// SAFETY: encoder references are atomic counts (obs-encoder.c:1974-1986, obs-internal.h:680-683); we drop ours on OBS threads.
unsafe impl Send for EncoderRef {}

impl EncoderRef {
    fn get(&self) -> *mut sys::obs_encoder_t {
        self.0
    }
}

impl Drop for EncoderRef {
    fn drop(&mut self) {
        // SAFETY: we hold one strong reference; the output holds its own (obs-output.c:1048-1049).
        unsafe { sys::obs_encoder_release(self.0) };
    }
}

/// A weak reference: it never keeps a stream encoder the frontend recreated alive.
pub struct WeakEncoder(*mut sys::obs_weak_encoder_t);

// SAFETY: as for EncoderRef; weak references are counted the same way.
unsafe impl Send for WeakEncoder {}

impl WeakEncoder {
    pub fn of(encoder: *mut sys::obs_encoder_t) -> Option<Self> {
        // SAFETY: a live encoder; this adds one weak reference (obs-encoder.c:2014-2022).
        let weak = unsafe { sys::obs_encoder_get_weak_encoder(encoder) };
        (!weak.is_null()).then_some(Self(weak))
    }

    /// The encoder if it still exists, as a strong reference for this start.
    fn upgrade(&self) -> Option<EncoderRef> {
        // SAFETY: we hold a weak reference; this returns NULL once the encoder is gone (obs-encoder.c:2024-2033).
        let strong = unsafe { sys::obs_weak_encoder_get_encoder(self.0) };
        (!strong.is_null()).then_some(EncoderRef(strong))
    }

    fn refers_to(&self, encoder: *mut sys::obs_encoder_t) -> bool {
        self.upgrade().is_some_and(|strong| strong.get() == encoder)
    }
}

impl Drop for WeakEncoder {
    fn drop(&mut self) {
        // SAFETY: we hold one weak reference.
        unsafe { sys::obs_weak_encoder_release(self.0) };
    }
}

#[derive(Default)]
pub struct Owned {
    audio: Option<EncoderRef>,
    audio_mixer: usize,
    video: Option<EncoderRef>,
    /// Without a frontend: the last stream encoder attached in share mode, so a start after a fallback checks it again.
    share: Option<WeakEncoder>,
}

struct WeakOutput(*mut sys::obs_weak_output_t);

// SAFETY: as for WeakEncoder; weak output references are counted atomically too.
unsafe impl Send for WeakOutput {}

impl WeakOutput {
    fn active(&self) -> bool {
        // SAFETY: we hold a weak reference; a strong one is taken, read and released here.
        unsafe {
            let output = sys::obs_weak_output_get_output(self.0);
            if output.is_null() {
                return false;
            }
            let active = sys::obs_output_active(output);
            sys::obs_output_release(output);
            active
        }
    }
}

impl Drop for WeakOutput {
    fn drop(&mut self) {
        // SAFETY: we hold one weak reference.
        unsafe { sys::obs_weak_output_release(self.0) };
    }
}

/// The frontend's streaming output and its video encoder, both weak, so a frontend rebuild frees them.
pub struct MainStream {
    output: WeakOutput,
    encoder: WeakEncoder,
    multitrack: bool,
}

impl MainStream {
    /// The stream's first video encoder and its id, if it has one.
    ///
    /// # Safety
    /// `output` is a live output the caller holds a reference to.
    pub unsafe fn of(output: *mut sys::obs_output_t) -> Option<(Self, String)> {
        // SAFETY: the caller's contract; the encoders are borrowed from the output while it is held.
        unsafe {
            let video = sys::obs_output_get_video_encoder(output);
            if video.is_null() {
                return None;
            }
            // The output type's multitrack flag only says what it can do: rtmp_output sets it for every stream.
            let multitrack = is_multitrack(
                !sys::obs_output_get_video_encoder2(output, 1).is_null(),
                &text(sys::obs_output_get_name(output)),
                &text(sys::obs_encoder_get_name(video)),
            );
            let encoder = WeakEncoder::of(video)?;
            let stream = Self {
                output: WeakOutput(sys::obs_output_get_weak_output(output)),
                encoder,
                multitrack,
            };
            Some((stream, text(sys::obs_encoder_get_id(video))))
        }
    }

    pub fn encoder(&self) -> &WeakEncoder {
        &self.encoder
    }

    pub fn multitrack(&self) -> bool {
        self.multitrack
    }
}

/// Enhanced Broadcasting may run one video track; OBS's names still mark it (MultitrackVideoOutput.cpp:134-152, :236).
fn is_multitrack(second_video_encoder: bool, output: &str, encoder: &str) -> bool {
    second_video_encoder
        || output.ends_with(" multitrack video")
        || encoder.starts_with("multitrack video video encoder")
}

/// Set from STREAMING_STARTING until STOPPING or STOPPED; a start that fails at once sends neither.
static MAIN_STREAM: Mutex<Option<MainStream>> = Mutex::new(None);

pub fn main_stream_starting(stream: Option<MainStream>) {
    *lock(&MAIN_STREAM) = stream;
}

/// Forgets the main stream; returns it if one was known.
pub fn main_stream_ending() -> Option<MainStream> {
    lock(&MAIN_STREAM).take()
}

/// What keeps the stream's encoder from being shared before its settings are checked.
fn stream_problems(running: bool, multitrack: bool) -> Vec<Problem> {
    if multitrack {
        vec![Problem::Multitrack]
    } else if !running {
        vec![Problem::StreamNotRunning]
    } else {
        Vec::new()
    }
}

fn main_stream() -> Option<(EncoderRef, Vec<Problem>)> {
    let guard = lock(&MAIN_STREAM);
    let stream = guard.as_ref()?;
    let encoder = stream.encoder.upgrade()?;
    Some((
        encoder,
        stream_problems(stream.output.active(), stream.multitrack),
    ))
}

/// Whether `output` encodes with `stream`'s encoder.
pub fn uses(output: OutputPtr, stream: &WeakEncoder) -> bool {
    // SAFETY: a live output; the encoder pointer is only compared.
    stream
        .upgrade()
        .is_some_and(|s| unsafe { sys::obs_output_get_video_encoder(output.0) } == s.get())
}

/// Drops the inactive output's reference to its video encoder, which may be the frontend's.
pub fn detach_video(output: OutputPtr) {
    // SAFETY: a live output; libobs refuses the change while it is active (obs-output.c:1024-1030).
    unsafe {
        if !sys::obs_output_active(output.0) {
            sys::obs_output_set_video_encoder(output.0, ptr::null_mut());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Choice {
    Share,
    Own,
    Refuse(String),
}

/// `candidate` is the stream encoder's problems, `None` without a stream encoder.
fn choose(candidate: Option<&[Problem]>, fallback: bool) -> Choice {
    let missing = [Problem::StreamNotRunning];
    let problems = candidate.unwrap_or(&missing);
    if problems.is_empty() {
        Choice::Share
    } else if fallback {
        Choice::Own
    } else {
        Choice::Refuse(messages(problems))
    }
}

struct Config {
    share: bool,
    video_encoder: String,
    bitrate_kbps: i64,
    fallback: bool,
    audio_track: i64,
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
                audio_track: sys::obs_data_get_int(s, c"audio_track".as_ptr()),
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
        parsed.map(|mut s| {
            s.format = Some(format_name(effective_format(encoder)).to_owned());
            s
        })
    }
}

/// Preferred format if set (a GPU mix uses it, obs-encoder.c:257; x264/QSV/VAAPI/NVENC read it), else the canvas's.
///
/// # Safety
/// `encoder` is a live video encoder with its video set.
unsafe fn effective_format(encoder: *mut sys::obs_encoder_t) -> sys::video_format {
    // SAFETY: the caller's contract.
    unsafe {
        let preferred = sys::obs_encoder_get_preferred_video_format(encoder);
        if preferred != sys::VIDEO_FORMAT_NONE {
            return preferred;
        }
        sys::video_output_get_info(sys::obs_encoder_video(encoder))
            .as_ref()
            .map_or(sys::VIDEO_FORMAT_NONE, |info| info.format)
    }
}

fn format_name(format: sys::video_format) -> &'static str {
    match format {
        sys::VIDEO_FORMAT_I420 => "I420",
        sys::VIDEO_FORMAT_NV12 => "NV12",
        sys::VIDEO_FORMAT_YVYU => "YVYU",
        sys::VIDEO_FORMAT_YUY2 => "YUY2",
        sys::VIDEO_FORMAT_UYVY => "UYVY",
        sys::VIDEO_FORMAT_RGBA => "RGBA",
        sys::VIDEO_FORMAT_BGRA => "BGRA",
        sys::VIDEO_FORMAT_BGRX => "BGRX",
        sys::VIDEO_FORMAT_Y800 => "Y800",
        sys::VIDEO_FORMAT_I444 => "I444",
        sys::VIDEO_FORMAT_BGR3 => "BGR3",
        sys::VIDEO_FORMAT_I422 => "I422",
        sys::VIDEO_FORMAT_I40A => "I40A",
        sys::VIDEO_FORMAT_I42A => "I42A",
        sys::VIDEO_FORMAT_YUVA => "YUVA",
        sys::VIDEO_FORMAT_AYUV => "AYUV",
        sys::VIDEO_FORMAT_I010 => "I010",
        sys::VIDEO_FORMAT_P010 => "P010",
        sys::VIDEO_FORMAT_I210 => "I210",
        sys::VIDEO_FORMAT_I412 => "I412",
        sys::VIDEO_FORMAT_YA2L => "YA2L",
        sys::VIDEO_FORMAT_P216 => "P216",
        sys::VIDEO_FORMAT_P416 => "P416",
        sys::VIDEO_FORMAT_V210 => "V210",
        sys::VIDEO_FORMAT_R10L => "R10L",
        _ => "NONE",
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

fn create_audio(mixer_idx: usize) -> Result<EncoderRef, String> {
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
            mixer_idx,
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
    let path = unsafe {
        let resize = (width, height) != (ovi.output_width, ovi.output_height);
        let convert =
            ![sys::VIDEO_FORMAT_NV12, sys::VIDEO_FORMAT_I420].contains(&ovi.output_format);
        if resize {
            sys::obs_encoder_set_scaled_size(encoder.get(), width, height);
        }
        if resize || convert {
            // The canvas's filter: the mix scales from the canvas's base size like the canvas output (obs-encoder.c:296-304).
            let scale = if ovi.scale_type == sys::OBS_SCALE_DISABLE {
                sys::OBS_SCALE_BICUBIC
            } else {
                ovi.scale_type
            };
            sys::obs_encoder_set_gpu_scale_type(encoder.get(), scale);
            // A GPU mix keeps the canvas format unless told (obs-encoder.c:257); texture encoders need NV12 (obs.c:150).
            sys::obs_encoder_set_preferred_video_format(encoder.get(), sys::VIDEO_FORMAT_NV12);
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
        if sys::obs_encoder_gpu_scaling_enabled(encoder.get()) {
            format!(
                "a GPU mix in {}",
                format_name(sys::obs_encoder_get_preferred_video_format(encoder.get()))
            )
        } else {
            "the canvas's video".to_owned()
        }
    };
    crate::hooks::observe("own_video_encoder", || {
        serde_json::json!({
            "id":config.video_encoder,"width":width,"height":height,"fps":fps,
            "gpu_mix": path.starts_with("a GPU mix"), "format": unsafe { format_name(effective_format(encoder.get())) }
        })
    });
    log::info(&format!(
        "own video encoder {} at {width}×{height}, {fps} FPS, {} kbps, from {path}",
        config.video_encoder, config.bitrate_kbps
    ));
    Ok(encoder)
}

/// Sets this start's encoders on the output, which is inactive; runs on the thread that called start.
pub fn prepare(output: OutputPtr, owned: &Mutex<Owned>) -> Result<(), String> {
    let config = Config::read(output);
    let mixer = settings::mixer_index(config.audio_track).map_err(|p| p.message())?;
    let mut owned = lock(owned);
    // SAFETY: the cached encoder is live while we hold its reference.
    let active = owned
        .audio
        .as_ref()
        .is_some_and(|a| unsafe { sys::obs_encoder_active(a.get()) });
    let bound = owned.audio.is_some().then_some(owned.audio_mixer);
    if settings::audio_plan(bound, mixer, active) != settings::AudioPlan::Keep {
        // The output still references the old encoder until set_audio_encoder below; it is inactive, so dropping ours is safe.
        owned.audio = Some(create_audio(mixer)?);
        owned.audio_mixer = mixer;
    }
    let audio = owned
        .audio
        .as_ref()
        .map(EncoderRef::get)
        .expect("the audio encoder is set above");
    // SAFETY: the output is inactive in start (obs-output.c:1024-1040 refuses otherwise) and the encoders are live.
    unsafe {
        let attached = sys::obs_output_get_audio_encoder(output.0, 0);
        if !attached.is_null() && attached != audio {
            crate::hooks::observe(
                "audio_encoder_replaced",
                || serde_json::json!({"previous":text(sys::obs_encoder_get_id(attached)),"current":"ffmpeg_opus"}),
            );
            log::info(&format!(
                "replacing the attached audio encoder {} with D-Streamy's own Opus encoder",
                text(sys::obs_encoder_get_id(attached))
            ));
        }
        sys::obs_output_set_audio_encoder(output.0, audio, 0);
    }
    let streaming = crate::frontend::follows_streaming();
    if !streaming {
        // SAFETY: as above.
        let attached = unsafe { sys::obs_output_get_video_encoder(output.0) };
        let ours = owned.video.as_ref().map(EncoderRef::get);
        let known_share = owned.share.as_ref().is_some_and(|w| w.refers_to(attached));
        if !attached.is_null() && Some(attached) != ours && !known_share {
            owned.share = WeakEncoder::of(attached);
        }
    }
    if config.share {
        let (candidate, stream) = if streaming {
            main_stream().unzip()
        } else {
            (
                owned.share.as_ref().and_then(WeakEncoder::upgrade),
                Some(Vec::new()),
            )
        };
        let problems = match (&candidate, stream) {
            (Some(c), Some(stream)) if stream.is_empty() => Some(share_problems(c.get())?),
            (Some(_), Some(stream)) => Some(stream),
            _ => None,
        };
        // SAFETY: the candidate is live while we hold its reference.
        let id = candidate
            .as_ref()
            .map(|c| unsafe { text(sys::obs_encoder_get_id(c.get())) });
        match choose(problems.as_deref(), config.fallback) {
            Choice::Share => {
                let (candidate, id) = (
                    candidate.expect("Share is chosen only with a candidate"),
                    id.unwrap_or_default(),
                );
                // SAFETY: the output is inactive; it takes its own reference.
                unsafe { sys::obs_output_set_video_encoder(output.0, candidate.get()) };
                crate::hooks::observe("shared_video_encoder", || serde_json::json!({"id":id}));
                log::info(&format!("sharing the stream's video encoder {id}"));
                return Ok(());
            }
            Choice::Refuse(message) => {
                detach_video(output);
                return Err(message);
            }
            Choice::Own => {
                let problems = problems.unwrap_or_else(|| vec![Problem::StreamNotRunning]);
                crate::hooks::observe("share_refused", || {
                    serde_json::json!({
                        "id":id, "stream":candidate.is_some(),
                        "b_frames":problems.iter().any(|p| matches!(p, Problem::BFrames { .. })),
                        "inactive":problems.iter().any(|p| matches!(p, Problem::StreamNotRunning)),
                        "multitrack":problems.contains(&Problem::Multitrack),
                        "color_format":problems.iter().any(|p| matches!(p, Problem::ColorFormat { .. })),
                        "custom_options":problems.iter().any(|p| matches!(p, Problem::CustomOptions { .. }))
                    })
                });
                let shared = id.unwrap_or_else(|| "(none)".into());
                log::warn(&format!(
                    "not sharing the stream's video encoder {shared} ({}), so D-Streamy uses its own",
                    messages(&problems)
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
            sys::obs_data_set_default_int(settings, c"audio_track".as_ptr(), 1);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multitrack_or_stopped_stream_is_not_shared() {
        assert_eq!(stream_problems(true, false), []);
        assert_eq!(stream_problems(false, false), [Problem::StreamNotRunning]);
        assert_eq!(stream_problems(true, true), [Problem::Multitrack]);
        assert_eq!(stream_problems(false, true), [Problem::Multitrack]);
        assert_eq!(
            choose(Some(&[Problem::Multitrack]), false),
            Choice::Refuse(Problem::Multitrack.message())
        );
        assert_eq!(choose(Some(&[Problem::Multitrack]), true), Choice::Own);
    }

    #[test]
    fn an_enhanced_broadcasting_stream_is_multitrack_even_with_one_encoder() {
        assert!(is_multitrack(true, "simple_stream", "streaming_h264"));
        assert!(is_multitrack(
            false,
            "rtmp multitrack video",
            "multitrack video video encoder 0"
        ));
        assert!(is_multitrack(false, "flv multitrack video", "x"));
        assert!(is_multitrack(
            false,
            "x",
            "multitrack video video encoder 0"
        ));
        assert!(!is_multitrack(false, "simple_stream", "simple_h264_stream"));
        assert!(!is_multitrack(false, "adv_stream", "advanced_video_stream"));
    }

    #[test]
    fn a_passing_stream_encoder_is_shared() {
        assert_eq!(choose(Some(&[]), false), Choice::Share);
        assert_eq!(choose(Some(&[]), true), Choice::Share);
    }

    #[test]
    fn without_a_stream_encoder_the_fallback_decides() {
        assert_eq!(choose(None, true), Choice::Own);
        assert_eq!(
            choose(None, false),
            Choice::Refuse(Problem::StreamNotRunning.message())
        );
    }

    #[test]
    fn a_failing_stream_encoder_falls_back_or_is_refused_with_its_problems() {
        let problems = [
            Problem::StreamNotRunning,
            Problem::BFrames {
                family: Family::X264,
            },
        ];
        assert_eq!(choose(Some(&problems), true), Choice::Own);
        assert_eq!(
            choose(Some(&problems), false),
            Choice::Refuse(messages(&problems))
        );
    }
}
