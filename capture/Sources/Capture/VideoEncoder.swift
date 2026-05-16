import VideoToolbox
import CoreMedia
import Foundation

public final class VideoEncoder {
    private let width: Int
    private let height: Int
    private let fps: Int
    private let bitrateBps: Int
    /// Callback receives a complete Annex B frame (all NALUs concatenated, SPS/PPS prepended on keyframes)
    public let onFrame: (Data, UInt64) -> Void

    private var session: VTCompressionSession?

    public init(width: Int, height: Int, fps: Int = 30, bitrateMbps: Double = 3.0,
         onFrame: @escaping (Data, UInt64) -> Void) {
        self.width = width
        self.height = height
        self.fps = fps
        self.bitrateBps = Int(bitrateMbps * 1_000_000)
        self.onFrame = onFrame
    }

    public func start() throws {
        var s: VTCompressionSession?
        var status = VTCompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            width: Int32(width),
            height: Int32(height),
            codecType: kCMVideoCodecType_H264,
            encoderSpecification: nil,
            imageBufferAttributes: nil,
            compressedDataAllocator: nil,
            outputCallback: nil,
            refcon: nil,
            compressionSessionOut: &s
        )
        guard status == noErr, let session = s else {
            throw EncoderError.sessionCreationFailed(status)
        }
        self.session = session

        // H.264 Baseline
        status = VTSessionSetProperty(session,
            key: kVTCompressionPropertyKey_ProfileLevel,
            value: kVTProfileLevel_H264_Baseline_AutoLevel)
        check(status, "ProfileLevel")

        // Average bitrate (CBR approximation)
        status = VTSessionSetProperty(session,
            key: kVTCompressionPropertyKey_AverageBitRate,
            value: bitrateBps as CFNumber)
        check(status, "AverageBitRate")

        let peakBps = Int(Double(bitrateBps) * 1.5)
        let dataRateLimits = [peakBps, 1] as CFArray
        status = VTSessionSetProperty(session,
            key: kVTCompressionPropertyKey_DataRateLimits,
            value: dataRateLimits)
        check(status, "DataRateLimits")

        // Real-time
        status = VTSessionSetProperty(session,
            key: kVTCompressionPropertyKey_RealTime,
            value: kCFBooleanTrue)
        check(status, "RealTime")

        // Keyframe every 2 seconds
        let keyInterval = fps * 2
        status = VTSessionSetProperty(session,
            key: kVTCompressionPropertyKey_MaxKeyFrameInterval,
            value: keyInterval as CFNumber)
        check(status, "MaxKeyFrameInterval")

        status = VTCompressionSessionPrepareToEncodeFrames(session)
        guard status == noErr else {
            throw EncoderError.prepareFailed(status)
        }
    }

    public func stop() {
        guard let session else { return }
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
        VTCompressionSessionInvalidate(session)
        self.session = nil
    }

    public func encode(pixelBuffer: CVPixelBuffer, presentationTime: CMTime) {
        encodeInternal(pixelBuffer: pixelBuffer, presentationTime: presentationTime, forceKey: false)
    }

    public func forceKeyframe(pixelBuffer: CVPixelBuffer, presentationTime: CMTime) {
        encodeInternal(pixelBuffer: pixelBuffer, presentationTime: presentationTime, forceKey: true)
    }

    // MARK: - Private

    private func encodeInternal(pixelBuffer: CVPixelBuffer, presentationTime: CMTime, forceKey: Bool) {
        guard let session else { return }

        var frameProps: CFDictionary?
        if forceKey {
            frameProps = [kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary
        }

        VTCompressionSessionEncodeFrame(
            session,
            imageBuffer: pixelBuffer,
            presentationTimeStamp: presentationTime,
            duration: .invalid,
            frameProperties: frameProps,
            infoFlagsOut: nil
        ) { [weak self] status, _, sampleBuffer in
            guard status == noErr,
                  let sampleBuffer,
                  CMSampleBufferDataIsReady(sampleBuffer),
                  let self else { return }
            self.handleOutput(sampleBuffer: sampleBuffer)
        }
    }

    private func handleOutput(sampleBuffer: CMSampleBuffer) {
        let pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        guard pts.isValid, !pts.seconds.isNaN, !pts.seconds.isInfinite else { return }
        let timestampUs = UInt64(max(0, pts.seconds * 1_000_000))

        guard let dataBuffer = CMSampleBufferGetDataBuffer(sampleBuffer) else { return }

        var totalLength = 0
        var dataPointer: UnsafeMutablePointer<Int8>?
        let result = CMBlockBufferGetDataPointer(
            dataBuffer,
            atOffset: 0,
            lengthAtOffsetOut: nil,
            totalLengthOut: &totalLength,
            dataPointerOut: &dataPointer
        )
        guard result == noErr, let dataPointer else { return }

        var frameData = Data()

        // Check if keyframe — if so, prepend SPS/PPS from format description
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false)
        var isKeyframe = true // assume keyframe if no attachments
        if let attachments, CFArrayGetCount(attachments) > 0 {
            let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFDictionary.self)
            if let notSync = CFDictionaryGetValue(dict, Unmanaged.passUnretained(kCMSampleAttachmentKey_NotSync).toOpaque()) {
                isKeyframe = !(unsafeBitCast(notSync, to: CFBoolean.self) == kCFBooleanTrue)
            }
        }

        if isKeyframe, let formatDesc = CMSampleBufferGetFormatDescription(sampleBuffer) {
            // Extract SPS
            var spsSize = 0
            var spsCount = 0
            var spsPtr: UnsafePointer<UInt8>?
            let spsStatus = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                formatDesc, parameterSetIndex: 0,
                parameterSetPointerOut: &spsPtr, parameterSetSizeOut: &spsSize,
                parameterSetCountOut: &spsCount, nalUnitHeaderLengthOut: nil
            )
            if spsStatus == noErr, let spsPtr {
                frameData.append(contentsOf: [0x00, 0x00, 0x00, 0x01])
                frameData.append(UnsafeBufferPointer(start: spsPtr, count: spsSize))
            }

            // Extract PPS
            var ppsSize = 0
            var ppsPtr: UnsafePointer<UInt8>?
            let ppsStatus = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                formatDesc, parameterSetIndex: 1,
                parameterSetPointerOut: &ppsPtr, parameterSetSizeOut: &ppsSize,
                parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil
            )
            if ppsStatus == noErr, let ppsPtr {
                frameData.append(contentsOf: [0x00, 0x00, 0x00, 0x01])
                frameData.append(UnsafeBufferPointer(start: ppsPtr, count: ppsSize))
            }
        }

        // Walk AVCC length-prefixed NALUs, convert to Annex B and concatenate
        var offset = 0
        while offset < totalLength - 4 {
            var naluLength: UInt32 = 0
            memcpy(&naluLength, dataPointer.advanced(by: offset), 4)
            naluLength = UInt32(bigEndian: naluLength)

            offset += 4
            guard offset + Int(naluLength) <= totalLength else { break }

            frameData.append(contentsOf: [0x00, 0x00, 0x00, 0x01])
            frameData.append(Data(bytes: dataPointer.advanced(by: offset), count: Int(naluLength)))

            offset += Int(naluLength)
        }

        // Emit complete frame as single callback
        onFrame(frameData, timestampUs)
    }

    @inline(__always)
    private func check(_ status: OSStatus, _ label: String) {
        if status != noErr {
            fputs("VideoEncoder: \(label) property failed (\(status))\n", stderr)
        }
    }

    public enum EncoderError: Error {
        case sessionCreationFailed(OSStatus)
        case prepareFailed(OSStatus)
    }
}
