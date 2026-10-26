//! `dstreamy_output`: an encoded A/V output that hands its packets to `dstreamy serve`.

use std::ffi::{CStr, CString, c_char, c_void};
use std::mem;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use dstreamy_obs_producer::{Hello, Packet, Ring, Track};
use dstreamy_obs_sys as sys;
use tokio::sync::mpsc;

use crate::guard::{self, guard, guard_with};
use crate::log;
use crate::worker::{self, Command};

const OUTPUT_ID: &CStr = c"dstreamy_output";
const OUTPUT_NAME: &CStr = c"D-Streamy";
const STATS_DECL: &CStr = c"void get_stats(out int video_packets, out int audio_packets, out int panics, out int dropped_video, out int dropped_audio)";

#[derive(Clone, Copy)]
pub struct OutputPtr(pub *mut sys::obs_output_t);

// SAFETY: plugin threads may call the output API (rtmp-stream.c's connect and send threads); destroy joins the worker.
unsafe impl Send for OutputPtr {}
// SAFETY: as above.
unsafe impl Sync for OutputPtr {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanicIn {
    Nowhere,
    Start,
    Stop,
}

/// What the libobs callbacks and the worker thread share.
pub struct Shared {
    pub output: OutputPtr,
    pub ring: Ring,
    pub commands: mpsc::UnboundedSender<Command>,
    pub encode_error: AtomicBool,
    /// Set by `start`, cleared by the worker once it signalled the stop.
    pub busy: AtomicBool,
    video: AtomicI64,
    audio: AtomicI64,
}

impl Shared {
    pub fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            log::warn("the D-Streamy worker thread is gone");
        }
    }

    fn fail(&self, message: &str) {
        self.send(Command::Fail(format!("D-Streamy plugin error ({message})")));
    }
}

struct Output {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    panic_at: i64,
    panic_in: PanicIn,
}

impl Output {
    fn maybe_panic(&self, at: PanicIn) {
        if self.panic_in == at {
            panic!("test panic in {at:?}");
        }
    }

    fn count(&self, packet: &sys::encoder_packet) {
        let shared = &self.shared;
        let counter = if packet.type_ == sys::OBS_ENCODER_VIDEO {
            &shared.video
        } else {
            &shared.audio
        };
        counter.fetch_add(1, Ordering::Relaxed);
        let seen = shared.video.load(Ordering::Relaxed) + shared.audio.load(Ordering::Relaxed);
        if self.panic_at > 0 && seen == self.panic_at {
            panic!("test panic at packet {seen}");
        }
    }

    /// The only work done on the encoder thread: one copy into the ring.
    fn enqueue(&self, packet: &sys::encoder_packet) {
        let data = if packet.data.is_null() || packet.size == 0 {
            Vec::new()
        } else {
            // SAFETY: libobs hands a packet whose `data` holds `size` bytes for the duration of the call.
            unsafe { std::slice::from_raw_parts(packet.data, packet.size) }.to_vec()
        };
        let track = if packet.type_ == sys::OBS_ENCODER_VIDEO {
            Track::Video
        } else {
            Track::Audio
        };
        self.shared.ring.push(Packet {
            track,
            keyframe: packet.keyframe,
            pts: packet.pts,
            dts: packet.dts,
            timebase: (packet.timebase_num, packet.timebase_den),
            data,
            arrived: Instant::now(),
        });
    }
}

/// # Safety
/// `data` is the pointer `output_create` returned and libobs has not destroyed it yet.
unsafe fn state<'a>(data: *mut c_void) -> &'a Output {
    // SAFETY: the caller's contract.
    unsafe { &*data.cast::<Output>() }
}

pub fn set_last_error(output: OutputPtr, message: &str) {
    let text = CString::new(message.replace('\0', " ")).expect("NUL bytes were replaced");
    // SAFETY: libobs copies the message (obs-output.c:3231-3242).
    unsafe { sys::obs_output_set_last_error(output.0, text.as_ptr()) };
}

/// # Safety
/// `ptr` is NULL or a NUL-terminated string valid for the call.
unsafe fn text(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: the caller's contract.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// # Safety
/// Both encoders belong to the output being started and are initialized.
unsafe fn hello(
    venc: *mut sys::obs_encoder_t,
    aenc: *mut sys::obs_encoder_t,
) -> Result<Hello, String> {
    // SAFETY: valid encoders; libobs returns static strings or NULL, and video info owned by the encoder's video.
    unsafe {
        let info = sys::video_output_get_info(sys::obs_encoder_video(venc));
        let info = info.as_ref().ok_or("the video encoder has no video")?;
        if info.fps_den == 0 || info.fps_num % info.fps_den != 0 {
            return Err(format!(
                "D-Streamy needs a whole frame rate (15, 30 or 60 fps), not {}/{}",
                info.fps_num, info.fps_den
            ));
        }
        let sample_rate = sys::obs_encoder_get_sample_rate(aenc);
        if sample_rate == 0 {
            return Err("the audio encoder has no sample rate".into());
        }
        let frame_size = sys::obs_encoder_get_frame_size(aenc) as u64;
        Ok(Hello {
            video_codec: text(sys::obs_encoder_get_codec(venc)),
            width: sys::obs_encoder_get_width(venc),
            height: sys::obs_encoder_get_height(venc),
            fps: info.fps_num / info.fps_den,
            audio_codec: text(sys::obs_encoder_get_codec(aenc)),
            sample_rate,
            channels: u32::try_from(sys::audio_output_get_channels(sys::obs_encoder_audio(aenc)))
                .unwrap_or(0),
            frame_ms: u32::try_from(frame_size * 1000 / u64::from(sample_rate)).unwrap_or(0),
            client_name: "obs-dstreamy".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            obs_version: text(sys::obs_get_version_string()),
        })
    }
}

fn endpoint_setting(output: OutputPtr) -> Option<PathBuf> {
    // SAFETY: obs_output_get_settings adds a reference, released below.
    unsafe {
        let settings = sys::obs_output_get_settings(output.0);
        let endpoint = text(sys::obs_data_get_string(settings, c"endpoint".as_ptr()));
        sys::obs_data_release(settings);
        (!endpoint.is_empty()).then(|| PathBuf::from(endpoint))
    }
}

extern "C" fn output_get_name(_type_data: *mut c_void) -> *const c_char {
    guard("get_name", OUTPUT_NAME.as_ptr(), || OUTPUT_NAME.as_ptr())
}

extern "C" fn output_create(
    settings: *mut sys::obs_data_t,
    output: *mut sys::obs_output_t,
) -> *mut c_void {
    guard("create", ptr::null_mut(), || {
        // SAFETY: libobs passes the output's live settings object.
        let (panic_at, panic_in) = unsafe {
            (
                sys::obs_data_get_int(settings, c"panic_at_packet".as_ptr()),
                text(sys::obs_data_get_string(settings, c"panic_in".as_ptr())),
            )
        };
        let panic_in = match panic_in.as_str() {
            "start" => PanicIn::Start,
            "stop" => PanicIn::Stop,
            _ => PanicIn::Nowhere,
        };
        let (commands, receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            output: OutputPtr(output),
            ring: Ring::default(),
            commands,
            encode_error: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            video: AtomicI64::new(0),
            audio: AtomicI64::new(0),
        });
        let worker = match worker::spawn(shared.clone(), receiver) {
            Ok(handle) => handle,
            Err(e) => {
                log::error(&format!("cannot start the D-Streamy worker thread: {e}"));
                return ptr::null_mut();
            }
        };
        let state = Box::into_raw(Box::new(Output {
            shared,
            worker: Mutex::new(Some(worker)),
            panic_at,
            panic_in,
        }));
        // SAFETY: the handler dies with `state` in obs_output_destroy (obs-output.c:308,344); libobs copies the decl.
        unsafe {
            let ph = sys::obs_output_get_proc_handler(output);
            sys::proc_handler_add(ph, STATS_DECL.as_ptr(), Some(get_stats), state.cast());
        }
        state.cast()
    })
}

extern "C" fn output_destroy(data: *mut c_void) {
    guard("destroy", (), || {
        // SAFETY: libobs calls destroy once, with the pointer create returned.
        let state = unsafe { Box::from_raw(data.cast::<Output>()) };
        state.shared.send(Command::Shutdown);
        let worker = state
            .worker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(worker) = worker
            && worker.join().is_err()
        {
            log::warn("the D-Streamy worker thread panicked");
        }
    });
}

extern "C" fn output_start(data: *mut c_void) -> bool {
    // SAFETY: libobs passes the pointer create returned.
    let state = unsafe { state(data) };
    let output = state.shared.output;
    guard_with(
        "start",
        false,
        || {
            state.maybe_panic(PanicIn::Start);
            let shared = &state.shared;
            if shared.busy.swap(true, Ordering::AcqRel) {
                set_last_error(output, "D-Streamy is already starting");
                return false;
            }
            let started = start(state);
            if !started {
                shared.busy.store(false, Ordering::Release);
            }
            started
        },
        |message| {
            state.shared.busy.store(false, Ordering::Release);
            set_last_error(output, &format!("D-Streamy plugin error ({message})"));
        },
    )
}

/// Initializes the encoders on the caller's thread, then hands the connect to the worker and returns.
fn start(state: &Output) -> bool {
    let shared = &state.shared;
    let output = shared.output;
    // SAFETY: `output` is the live output being started; the encoders it returns are owned by it.
    let encoders = unsafe {
        if !sys::obs_output_can_begin_data_capture(output.0, 0)
            || !sys::obs_output_initialize_encoders(output.0, 0)
        {
            return false;
        }
        let venc = sys::obs_output_get_video_encoder(output.0);
        let aenc = sys::obs_output_get_audio_encoder(output.0, 0);
        (!venc.is_null() && !aenc.is_null()).then(|| hello(venc, aenc))
    };
    let hello = match encoders {
        Some(Ok(hello)) => hello,
        Some(Err(message)) => {
            set_last_error(output, &message);
            return false;
        }
        None => {
            set_last_error(output, "D-Streamy needs a video and an audio encoder");
            return false;
        }
    };
    shared.ring.reopen();
    shared.encode_error.store(false, Ordering::Release);
    shared.video.store(0, Ordering::Relaxed);
    shared.audio.store(0, Ordering::Relaxed);
    let endpoint = endpoint_setting(output);
    if shared
        .commands
        .send(Command::Start { hello, endpoint })
        .is_err()
    {
        set_last_error(output, "the D-Streamy worker thread is gone");
        return false;
    }
    true
}

extern "C" fn output_stop(data: *mut c_void, _ts: u64) {
    // SAFETY: libobs passes the pointer create returned.
    let state = unsafe { state(data) };
    guard_with(
        "stop",
        (),
        || {
            state.maybe_panic(PanicIn::Stop);
            state.shared.send(Command::Stop);
        },
        |message| state.shared.fail(message),
    );
}

extern "C" fn output_encoded_packet(data: *mut c_void, packet: *mut sys::encoder_packet) {
    // SAFETY: libobs passes the pointer create returned.
    let state = unsafe { state(data) };
    guard_with(
        "encoded_packet",
        (),
        || {
            // SAFETY: a non-null packet is valid for the duration of the call.
            match unsafe { packet.as_ref() } {
                Some(packet) => {
                    state.count(packet);
                    state.enqueue(packet);
                }
                // Never signal here: this thread can hold video input_mutex. The worker reports it.
                None => {
                    state.shared.encode_error.store(true, Ordering::Release);
                    state.shared.send(Command::EncodeError);
                }
            }
        },
        |message| state.shared.fail(message),
    );
}

extern "C" fn get_stats(data: *mut c_void, cd: *mut sys::calldata_t) {
    guard("get_stats", (), || {
        // SAFETY: registered in create with the state pointer; callers only call it on a live output.
        let shared = unsafe { &state(data).shared };
        let ring = shared.ring.stats();
        let values = [
            (c"video_packets", shared.video.load(Ordering::Relaxed)),
            (c"audio_packets", shared.audio.load(Ordering::Relaxed)),
            (c"panics", guard::panics()),
            (
                c"dropped_video",
                i64::try_from(ring.dropped_video).unwrap_or(i64::MAX),
            ),
            (
                c"dropped_audio",
                i64::try_from(ring.dropped_audio).unwrap_or(i64::MAX),
            ),
        ];
        for (name, value) in values {
            // SAFETY: calldata stores an `int` out parameter as a long long (calldata.h:165-168).
            unsafe {
                sys::calldata_set_data(
                    cd,
                    name.as_ptr(),
                    (&raw const value).cast(),
                    mem::size_of::<i64>(),
                );
            }
        }
    });
}

pub fn register() {
    let info = sys::obs_output_info {
        id: OUTPUT_ID.as_ptr(),
        flags: sys::OBS_OUTPUT_AV | sys::OBS_OUTPUT_ENCODED,
        get_name: Some(output_get_name),
        create: Some(output_create),
        destroy: Some(output_destroy),
        start: Some(output_start),
        stop: Some(output_stop),
        encoded_packet: Some(output_encoded_packet),
        encoded_video_codecs: c"h264".as_ptr(),
        encoded_audio_codecs: c"opus".as_ptr(),
        ..Default::default()
    };
    // SAFETY: libobs copies `info` (`obs-module.c:894-912`); the strings it points to are 'static.
    unsafe { sys::obs_register_output_s(&info, mem::size_of::<sys::obs_output_info>()) };
}
