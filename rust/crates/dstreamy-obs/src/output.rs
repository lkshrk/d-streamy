//! `dstreamy_output`: an encoded A/V output that counts packets.

use std::ffi::{CStr, c_char, c_void};
use std::mem;
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread::JoinHandle;

use dstreamy_obs_sys as sys;

use crate::guard::{self, guard};

const OUTPUT_ID: &CStr = c"dstreamy_output";
const STATS_DECL: &CStr =
    c"void get_stats(out int video_packets, out int audio_packets, out int panics)";

struct OutputHandle(*mut sys::obs_output_t);

// SAFETY: libobs output functions synchronise internally; the stop thread mirrors null-output.c.
unsafe impl Send for OutputHandle {}

struct Output {
    output: *mut sys::obs_output_t,
    video: AtomicI64,
    audio: AtomicI64,
    panic_at: i64,
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
        let counter = if packet.type_ == sys::OBS_ENCODER_VIDEO {
            &self.video
        } else {
            &self.audio
        };
        let seen = self.video.load(Ordering::Relaxed) + self.audio.load(Ordering::Relaxed) + 1;
        if self.panic_at > 0 && seen == self.panic_at {
            counter.fetch_add(1, Ordering::Relaxed);
            panic!("test panic at packet {seen}");
        }
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// # Safety
/// `data` is the pointer `output_create` returned and libobs has not destroyed it yet.
unsafe fn state<'a>(data: *mut c_void) -> &'a Output {
    // SAFETY: the caller's contract.
    unsafe { &*data.cast::<Output>() }
}

extern "C" fn output_get_name(_type_data: *mut c_void) -> *const c_char {
    guard("get_name", ptr::null(), || c"D-Streamy".as_ptr())
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
            video: AtomicI64::new(0),
            audio: AtomicI64::new(0),
            panic_at,
            stop_thread: Mutex::new(None),
        }));
        // SAFETY: `state` lives until output_destroy, which libobs calls before freeing the proc handler.
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
        // SAFETY: `state.output` is the live output this state belongs to.
        unsafe {
            if !sys::obs_output_can_begin_data_capture(state.output, 0)
                || !sys::obs_output_initialize_encoders(state.output, 0)
            {
                return false;
            }
        }
        state.join_stop_thread();
        // SAFETY: as above.
        unsafe { sys::obs_output_begin_data_capture(state.output, 0) }
    })
}

extern "C" fn output_stop(data: *mut c_void, _ts: u64) {
    guard("stop", (), || {
        // SAFETY: libobs passes the pointer create returned.
        let state = unsafe { state(data) };
        let output = OutputHandle(state.output);
        // libobs joins its own end-capture thread, so ending capture on the caller's thread can deadlock.
        let handle = std::thread::spawn(move || {
            let output = output;
            // SAFETY: destroy joins this thread before the output is freed.
            unsafe { sys::obs_output_end_data_capture(output.0) };
        });
        let previous = state
            .stop_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(handle);
        if let Some(previous) = previous {
            let _ = previous.join();
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
            // SAFETY: a NULL packet signals an encoder error (`obs-encoder.c:1380-1382`).
            None => unsafe {
                sys::obs_output_signal_stop(state.output, sys::OBS_OUTPUT_ENCODE_ERROR)
            },
        }
    });
}

extern "C" fn get_stats(data: *mut c_void, cd: *mut sys::calldata_t) {
    guard("get_stats", (), || {
        // SAFETY: registered in create with the state pointer, which outlives the proc handler call.
        let state = unsafe { state(data) };
        let values = [
            (c"video_packets", state.video.load(Ordering::Relaxed)),
            (c"audio_packets", state.audio.load(Ordering::Relaxed)),
            (c"panics", guard::panics()),
        ];
        for (name, value) in values {
            let value: i64 = value;
            // SAFETY: the declaration types each out parameter as `int`, which calldata stores as a long long.
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
