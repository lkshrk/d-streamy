use std::ffi::{c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use dstreamy_media::{EncodedVideoFrame, MediaError, VideoCodec, VideoEncoder, annexb};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime, CMTimeFlags, CMVideoCodecType,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex, kCMSampleAttachmentKey_NotSync,
    kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::CVPixelBuffer;
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_Baseline_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
};

const MICROS: i32 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VtEncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub codec: VideoCodec,
}

type Sink = Box<dyn FnMut(EncodedVideoFrame) + Send>;

enum Output {
    Queue(Vec<EncodedVideoFrame>),
    Sink(Sink),
}

struct Shared {
    codec: VideoCodec,
    output: Mutex<Output>,
    lost_frame: AtomicBool,
}

impl Shared {
    fn deliver(&self, frame: EncodedVideoFrame) {
        match &mut *self.output.lock().unwrap_or_else(PoisonError::into_inner) {
            Output::Queue(queue) => queue.push(frame),
            Output::Sink(sink) => sink(frame),
        }
    }

    fn drain(&self) -> Vec<EncodedVideoFrame> {
        match &mut *self.output.lock().unwrap_or_else(PoisonError::into_inner) {
            Output::Queue(queue) => std::mem::take(queue),
            Output::Sink(_) => Vec::new(),
        }
    }
}

/// Realtime VideoToolbox H.264 Baseline / H.265 Main encoder; `encode` returns the AUs completed so far.
pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    force_keyframe: CFRetained<CFDictionary>,
    shared: NonNull<Shared>,
    codec: VideoCodec,
}

// SAFETY: a compression session may be driven from any thread, and `Shared` is `Sync`.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    pub fn new(config: VtEncoderConfig) -> Result<Self, MediaError> {
        Self::create(config, Output::Queue(Vec::new()))
    }

    /// Delivers each access unit to `sink` on a VideoToolbox thread as soon as it is ready.
    pub fn with_sink(
        config: VtEncoderConfig,
        sink: impl FnMut(EncodedVideoFrame) + Send + 'static,
    ) -> Result<Self, MediaError> {
        Self::create(config, Output::Sink(Box::new(sink)))
    }

    fn create(config: VtEncoderConfig, output: Output) -> Result<Self, MediaError> {
        let VtEncoderConfig {
            width,
            height,
            fps,
            bitrate_bps,
            codec,
        } = config;
        let invalid = |what: &str| MediaError::InvalidConfig(format!("{what} in {config:?}"));
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(invalid("size must be non-zero and even"));
        }
        let fps = i32::try_from(fps)
            .ok()
            .filter(|&f| f > 0)
            .ok_or_else(|| invalid("fps"))?;
        let bitrate = i32::try_from(bitrate_bps)
            .ok()
            .filter(|&b| b > 0)
            .ok_or_else(|| invalid("bitrate"))?;
        let (w, h) = (
            i32::try_from(width).map_err(|_| invalid("width"))?,
            i32::try_from(height).map_err(|_| invalid("height"))?,
        );

        let shared = NonNull::from(Box::leak(Box::new(Shared {
            codec,
            output: Mutex::new(output),
            lost_frame: AtomicBool::new(false),
        })));
        let refcon = shared.as_ptr().cast::<c_void>();
        let mut raw: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: `on_output` matches the callback ABI and `refcon` outlives the session (see Drop).
        let status = unsafe {
            VTCompressionSession::create(
                None,
                w,
                h,
                codec_type(codec),
                None,
                None,
                None,
                Some(on_output),
                refcon,
                NonNull::from(&mut raw),
            )
        };
        let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
            // SAFETY: no session exists, so nothing else references `shared`.
            drop(unsafe { Box::from_raw(shared.as_ptr()) });
            return Err(MediaError::Encoder(format!(
                "VTCompressionSessionCreate({codec:?}) failed: {status}"
            )));
        };
        // SAFETY: Create returns a +1 reference that we now own.
        let session = unsafe { CFRetained::from_raw(raw) };

        let force_keyframe = CFDictionary::<CFString, CFBoolean>::from_slices(
            // SAFETY: framework constant, valid for the process lifetime.
            &[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }],
            &[CFBoolean::new(true)],
        );
        let encoder = Self {
            session,
            // SAFETY: a typed dictionary is the same CFDictionary object.
            force_keyframe: unsafe { CFRetained::cast_unchecked(force_keyframe) },
            shared,
            codec,
        };
        encoder.configure(fps, bitrate)?;
        // SAFETY: the session is valid.
        let status = unsafe { encoder.session.prepare_to_encode_frames() };
        if status != 0 {
            return Err(MediaError::Encoder(format!(
                "VTCompressionSessionPrepareToEncodeFrames failed: {status}"
            )));
        }
        Ok(encoder)
    }

    fn configure(&self, fps: i32, bitrate: i32) -> Result<(), MediaError> {
        let profile = match self.codec {
            // SAFETY: framework constants, valid for the process lifetime.
            VideoCodec::H264 => unsafe { kVTProfileLevel_H264_Baseline_AutoLevel },
            VideoCodec::H265 => unsafe { kVTProfileLevel_HEVC_Main_AutoLevel },
        };
        let peak_bytes_per_second = i64::from(bitrate) * 3 / 2 / 8;
        let data_rate_limits = CFArray::<CFNumber>::from_retained_objects(&[
            CFNumber::new_i64(peak_bytes_per_second),
            CFNumber::new_i32(1),
        ]);
        let truth: &CFType = CFBoolean::new(true);
        let falsity: &CFType = CFBoolean::new(false);
        // SAFETY: framework constants, valid for the process lifetime.
        let required: [(&CFString, &CFType); 5] = unsafe {
            [
                (kVTCompressionPropertyKey_ProfileLevel, profile),
                (
                    kVTCompressionPropertyKey_AverageBitRate,
                    &CFNumber::new_i32(bitrate),
                ),
                (kVTCompressionPropertyKey_RealTime, truth),
                (kVTCompressionPropertyKey_AllowFrameReordering, falsity),
                (
                    kVTCompressionPropertyKey_MaxKeyFrameInterval,
                    &CFNumber::new_i32(fps * 2),
                ),
            ]
        };
        for (key, value) in required {
            let status = self.set_property(key, value);
            if status != 0 {
                return Err(MediaError::Encoder(format!(
                    "VTSessionSetProperty({key}) failed: {status}"
                )));
            }
        }
        // SAFETY: framework constants; failures are ignored as some encoders reject these optional rate hints.
        unsafe {
            self.set_property(kVTCompressionPropertyKey_DataRateLimits, &data_rate_limits);
            self.set_property(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                &CFNumber::new_i32(fps),
            );
        }
        Ok(())
    }

    fn shared(&self) -> &Shared {
        // SAFETY: `shared` is freed only in Drop, after the session is invalidated.
        unsafe { self.shared.as_ref() }
    }

    fn set_property(&self, key: &CFString, value: &CFType) -> i32 {
        // SAFETY: the session is valid and `value` has the type the key documents.
        unsafe { VTSessionSetProperty(&self.session, key, Some(value)) }
    }
}

impl VideoEncoder for VtEncoder {
    type Frame = CVPixelBuffer;

    fn codec(&self) -> VideoCodec {
        self.codec
    }

    fn encode(
        &mut self,
        frame: &CVPixelBuffer,
        pts_us: u64,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedVideoFrame>, MediaError> {
        let pts = CMTime {
            value: i64::try_from(pts_us).unwrap_or(i64::MAX),
            timescale: MICROS,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let force = force_keyframe | self.shared().lost_frame.swap(false, Ordering::Relaxed);
        let options = force.then_some(&*self.force_keyframe);
        // SAFETY: session and frame are valid; VideoToolbox retains the frame while encoding.
        let status = unsafe {
            self.session.encode_frame(
                frame,
                pts,
                kCMTimeInvalid,
                options,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(MediaError::Encoder(format!(
                "VTCompressionSessionEncodeFrame failed: {status}"
            )));
        }
        Ok(self.shared().drain())
    }

    fn flush(&mut self) -> Result<Vec<EncodedVideoFrame>, MediaError> {
        // SAFETY: the session is valid.
        let status = unsafe { self.session.complete_frames(kCMTimeInvalid) };
        if status != 0 {
            return Err(MediaError::Encoder(format!(
                "VTCompressionSessionCompleteFrames failed: {status}"
            )));
        }
        Ok(self.shared().drain())
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // SAFETY: after invalidation no callback can reach `shared`, which was leaked in `create`.
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
            drop(Box::from_raw(self.shared.as_ptr()));
        }
    }
}

fn codec_type(codec: VideoCodec) -> CMVideoCodecType {
    match codec {
        VideoCodec::H264 => kCMVideoCodecType_H264,
        VideoCodec::H265 => kCMVideoCodecType_HEVC,
    }
}

unsafe extern "C-unwind" fn on_output(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: `refcon` is the encoder's `Shared`, alive until the session is invalidated.
    let shared = unsafe { &*refcon.cast::<Shared>() };
    // Unwinding into VideoToolbox would be undefined behaviour.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: a non-null `sample` is valid for the duration of the callback.
        let frame = (status == 0 && !sample.is_null())
            .then(|| annexb_frame(shared.codec, unsafe { &*sample }))
            .flatten();
        match frame {
            Some(frame) => shared.deliver(frame),
            None => shared.lost_frame.store(true, Ordering::Relaxed),
        }
    }));
}

fn annexb_frame(codec: VideoCodec, sample: &CMSampleBuffer) -> Option<EncodedVideoFrame> {
    // SAFETY: `sample` is a valid sample buffer for every call below.
    unsafe {
        if !sample.data_is_ready() {
            return None;
        }
        let pts_us = cmtime_to_us(sample.presentation_time_stamp())?;
        let keyframe = is_sync_sample(sample);
        let mut data = Vec::new();
        if keyframe {
            let format = sample.format_description()?;
            if append_parameter_sets(codec, &format, &mut data) != Some(4) {
                return None;
            }
        }
        let block = sample.data_buffer()?;
        let len = block.data_length();
        let mut avcc = vec![0u8; len];
        if len == 0 || block.copy_data_bytes(0, len, NonNull::from(avcc.as_mut_slice()).cast()) != 0
        {
            return None;
        }
        annexb::append_avcc_nalus(&avcc, &mut data);
        Some(EncodedVideoFrame {
            codec,
            data,
            pts_us,
            keyframe,
        })
    }
}

/// Converts a valid, non-negative `CMTime` to microseconds.
pub(crate) fn cmtime_to_us(time: CMTime) -> Option<u64> {
    if !time.flags.contains(CMTimeFlags::Valid) || time.timescale <= 0 {
        return None;
    }
    let us = i128::from(time.value) * 1_000_000 / i128::from(time.timescale);
    Some(u64::try_from(us.max(0)).unwrap_or(u64::MAX))
}

// SAFETY: `sample` must be a valid sample buffer.
unsafe fn is_sync_sample(sample: &CMSampleBuffer) -> bool {
    // SAFETY: valid sample; the array and its dictionaries are owned by the sample.
    unsafe {
        let Some(attachments) = sample.sample_attachments_array(false) else {
            return true;
        };
        let attachments: CFRetained<CFArray<CFDictionary<CFString, CFType>>> =
            CFRetained::cast_unchecked(attachments);
        let Some(first) = attachments.get(0) else {
            return true;
        };
        match first.get(kCMSampleAttachmentKey_NotSync) {
            Some(value) => !value
                .downcast_ref::<CFBoolean>()
                .is_some_and(|not_sync| not_sync.as_bool()),
            None => true,
        }
    }
}

/// Appends the parameter sets and returns the AVCC NAL length size.
// SAFETY: `format` must be a valid video format description of `codec`.
unsafe fn append_parameter_sets(
    codec: VideoCodec,
    format: &CMFormatDescription,
    out: &mut Vec<u8>,
) -> Option<i32> {
    let get = match codec {
        VideoCodec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
        VideoCodec::H265 => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    };
    let mut count = 0usize;
    let mut length_size: c_int = 0;
    // SAFETY: out-pointers are valid or null as the API allows.
    let status = unsafe {
        get(
            format,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut count,
            &mut length_size,
        )
    };
    if status != 0 {
        return None;
    }
    for index in 0..count {
        let mut set: *const u8 = ptr::null();
        let mut size = 0usize;
        // SAFETY: as above; the returned bytes are owned by `format`.
        let status = unsafe {
            get(
                format,
                index,
                &mut set,
                &mut size,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != 0 || set.is_null() {
            return None;
        }
        // SAFETY: VideoToolbox reports `size` readable bytes at `set`.
        annexb::append_nalu(out, unsafe { std::slice::from_raw_parts(set, size) });
    }
    Some(length_size)
}
