//! `dstreamy_output`: an encoded A/V output that counts packets.

use std::ffi::{CStr, c_char, c_void};
use std::mem;
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use dstreamy_obs_sys as sys;

use crate::guard::{self, guard};
use crate::log;

const OUTPUT_ID: &CStr = c"dstreamy_output";
const OUTPUT_NAME: &CStr = c"D-Streamy";
const STATS_DECL: &CStr =
    c"void get_stats(out int video_packets, out int audio_packets, out int panics)";

struct OutputHandle(*mut sys::obs_output_t);

// SAFETY: obs_output_end_data_capture may be called from any thread (null-output.c:66-80 does).
unsafe impl Send for OutputHandle {}

#[derive(Default)]
struct Stats {
    video: AtomicI64,
    audio: AtomicI64,
}

struct Output {
    output: *mut sys::obs_output_t,
    stats: Stats,
    panic_at: i64,
    started: Mutex<Option<Instant>>,
    stop_thread: Mutex<Option<JoinHandle<()>>>,
}

impl Output {
    fn join_stop_thread(&self) {
        let handle = self
            .stop_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    fn count(&self, packet: &sys::encoder_packet) {
        let stats = &self.stats;
        let counter = if packet.type_ == sys::OBS_ENCODER_VIDEO {
            &stats.video
        } else {
            &stats.audio
        };
        let seen = stats.video.load(Ordering::Relaxed) + stats.audio.load(Ordering::Relaxed) + 1;
        if counter.fetch_add(1, Ordering::Relaxed) == 0 {
            let started = *self.started.lock().unwrap_or_else(|e| e.into_inner());
            let kind = if packet.type_ == sys::OBS_ENCODER_VIDEO {
                "video"
            } else {
                "audio"
            };
            let ms = started.map_or(-1, |t| t.elapsed().as_millis() as i64);
            log::info(&format!("first {kind} packet {ms} ms after start"));
        }
        if self.panic_at > 0 && seen == self.panic_at {
            panic!("test panic at packet {seen}");
        }
    }
}

/// # Safety
/// `data` is the pointer `output_create` returned and libobs has not destroyed it yet.
unsafe fn state<'a>(data: *mut c_void) -> &'a Output {
    // SAFETY: the caller's contract.
    unsafe { &*data.cast::<Output>() }
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
        let panic_at = unsafe { sys::obs_data_get_int(settings, c"panic_at_packet".as_ptr()) };
        let state = Box::into_raw(Box::new(Output {
            output,
            stats: Stats::default(),
            panic_at,
            started: Mutex::new(None),
            stop_thread: Mutex::new(None),
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
        state.join_stop_thread();
    });
}

extern "C" fn output_start(data: *mut c_void) -> bool {
    guard("start", false, || {
        // SAFETY: libobs passes the pointer create returned.
        let state = unsafe { state(data) };
        state.join_stop_thread();
        *state.started.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        // SAFETY: `state.output` is the live output this state belongs to.
        let (can_begin, initialized, began) = unsafe {
            let can_begin = sys::obs_output_can_begin_data_capture(state.output, 0);
            let initialized = can_begin && sys::obs_output_initialize_encoders(state.output, 0);
            let began = initialized && sys::obs_output_begin_data_capture(state.output, 0);
            (can_begin, initialized, began)
        };
        log::info(&format!(
            "start: can_begin_data_capture={can_begin} initialize_encoders={initialized} begin_data_capture={began}"
        ));
        began
    })
}

extern "C" fn output_stop(data: *mut c_void, _ts: u64) {
    guard("stop", (), || {
        // SAFETY: libobs passes the pointer create returned.
        let state = unsafe { state(data) };
        let output = OutputHandle(state.output);
        // Ends capture off the caller's thread, following null_output (null-output.c:66-80).
        let spawned = std::thread::Builder::new()
            .name("dstreamy stop".into())
            .spawn(move || {
                let output = output;
                // SAFETY: destroy joins this thread before the output is freed.
                unsafe { sys::obs_output_end_data_capture(output.0) };
            });
        match spawned {
            Ok(handle) => {
                let previous = state
                    .stop_thread
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .replace(handle);
                if let Some(previous) = previous {
                    let _ = previous.join();
                }
            }
            Err(e) => {
                log::error(&format!(
                    "cannot start the stop thread ({e}); ending capture inline, which can block if called from full_stop"
                ));
                // SAFETY: `state.output` is live for the duration of this callback.
                unsafe { sys::obs_output_end_data_capture(state.output) };
            }
        }
    });
}

extern "C" fn output_encoded_packet(data: *mut c_void, packet: *mut sys::encoder_packet) {
    guard("encoded_packet", (), || {
        // SAFETY: libobs passes the pointer create returned.
        let state = unsafe { state(data) };
        // SAFETY: a non-null packet is valid for the duration of the call.
        match unsafe { packet.as_ref() } {
            Some(packet) => state.count(packet),
            // Unlike rtmp and ffmpeg-mux, never signal here: this thread holds video input_mutex.
            None => log::error("encoder error: the output is being stopped"),
        }
    });
}

extern "C" fn get_stats(data: *mut c_void, cd: *mut sys::calldata_t) {
    guard("get_stats", (), || {
        // SAFETY: registered in create with the state pointer; callers only call it on a live output.
        let stats = unsafe { &state(data).stats };
        let values = [
            (c"video_packets", stats.video.load(Ordering::Relaxed)),
            (c"audio_packets", stats.audio.load(Ordering::Relaxed)),
            (c"panics", guard::panics()),
        ];
        for (name, value) in values {
            let value: i64 = value;
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
