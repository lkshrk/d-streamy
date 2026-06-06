import Foundation
import CoreMedia
import AVFoundation
import ScreenCaptureKit
import OSLog

private let captureLog = Logger(subsystem: "me.harke.d-streamy", category: "capture")

/// Configuration for a capture session.
public struct CaptureConfig {
    public var filter: SCContentFilter
    public var sourceWidth: Int
    public var sourceHeight: Int
    public var maxWidth: Int
    public var maxHeight: Int
    public var fps: Int
    public var bitrateMbps: Double
    public var crop: CropFilter?
    public var audioGain: Float
    public var codec: VideoCodec
    public var outputFd: Int32

    public init(
        filter: SCContentFilter,
        sourceWidth: Int,
        sourceHeight: Int,
        maxWidth: Int = 1280,
        maxHeight: Int = 720,
        fps: Int = 30,
        bitrateMbps: Double = 6.0,
        crop: CropFilter? = nil,
        audioGain: Float = 1.0,
        codec: VideoCodec = .h264,
        outputFd: Int32 = STDOUT_FILENO
    ) {
        self.filter = filter
        self.sourceWidth = sourceWidth
        self.sourceHeight = sourceHeight
        self.maxWidth = maxWidth
        self.maxHeight = maxHeight
        self.fps = fps
        self.bitrateMbps = bitrateMbps
        self.crop = crop
        self.audioGain = audioGain
        self.codec = codec
        self.outputFd = outputFd
    }
}

/// Manages a window capture + encode + pipe session.
public final class CaptureSession {
    private var capture: WindowCapture?
    private var encoder: VideoEncoder?
    private var pipeWriter: PipeWriter?
    private var currentFps: Int = 30
    private var currentBitrate: Double = 6.0
    private var currentCodec: VideoCodec = .h264
    private var didLogAudioFormat = false

    public private(set) var captureWidth: Int = 0
    public private(set) var captureHeight: Int = 0
    public private(set) var isRunning = false
    public let metrics = CaptureMetrics()

    /// Live-updatable crop. Reads are on the frame callback thread.
    public private(set) var activeCrop: CropFilter?

    public init() {}

    /// Start capturing. Call from an async context.
    public func start(config: CaptureConfig) async throws {
        let dimensions = CaptureDimensions.resolve(
            sourceWidth: config.sourceWidth,
            sourceHeight: config.sourceHeight,
            maxWidth: config.maxWidth,
            maxHeight: config.maxHeight,
            crop: config.crop
        )
        captureWidth = dimensions.outputWidth
        captureHeight = dimensions.outputHeight

        self.currentFps = config.fps
        self.currentBitrate = config.bitrateMbps
        self.currentCodec = config.codec

        let writer = PipeWriter(fd: config.outputFd)
        self.pipeWriter = writer

        let enc = VideoEncoder(
            width: captureWidth,
            height: captureHeight,
            fps: config.fps,
            bitrateMbps: config.bitrateMbps,
            codec: config.codec
        ) { frameData, timestampUs in
            writer.write(type: .video, timestamp: timestampUs, data: frameData)
        }
        try enc.start()
        self.encoder = enc

        let cap = WindowCapture()
        self.capture = cap
        didLogAudioFormat = false

        // Set initial crop
        self.activeCrop = config.crop

        // Video — capture at full source resolution, crop per-frame
        cap.onVideoFrame = { [weak self] pixelBuffer, pts in
            guard let self, let encoder = self.encoder else { return }
            self.metrics.recordDelivered()
            let buf: CVPixelBuffer
            if let crop = self.activeCrop {
                guard let cropped = crop.apply(to: pixelBuffer) else {
                    self.metrics.recordEncodeDrop()
                    return
                }
                buf = cropped
            } else {
                buf = pixelBuffer
            }
            self.metrics.recordEncodeSubmit()
            encoder.encode(pixelBuffer: buf, presentationTime: pts)
        }

        // Audio
        let audioGain = config.audioGain
        cap.onAudioBuffer = { [weak self] sampleBuffer in
            let pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
            guard pts.isValid, !pts.seconds.isNaN, !pts.seconds.isInfinite else { return }
            let timestampUs = UInt64(max(0, pts.seconds * 1_000_000))

            let conversion = AudioPCMConverter.makeStereoPCM16Data(from: sampleBuffer, gain: audioGain)
            guard let int16Data = conversion.data else {
                self?.metrics.recordAudioConvFail()
                if self?.didLogAudioFormat == false {
                    captureLog.error("audio conversion failed")
                    StreamFileLogger.shared.write(component: "capture", "audio conversion failed")
                    self?.didLogAudioFormat = true
                }
                return
            }

            self?.metrics.recordAudioBuffer()

            if self?.didLogAudioFormat == false {
                captureLog.info(
                    "audio format samples=\(conversion.sampleCount, privacy: .public) buffers=\(conversion.bufferCount, privacy: .public) channels=\(conversion.sourceChannelCount, privacy: .public) interleaved=\(conversion.isInterleaved, privacy: .public) bytes=\(int16Data.count, privacy: .public)"
                )
                StreamFileLogger.shared.write(
                    component: "capture",
                    "audio format samples=\(conversion.sampleCount) buffers=\(conversion.bufferCount) channels=\(conversion.sourceChannelCount) interleaved=\(conversion.isInterleaved) bytes=\(int16Data.count)"
                )
                self?.didLogAudioFormat = true
            }
            writer.write(type: .audio, timestamp: timestampUs, data: int16Data)
        }

        do {
            try Task.checkCancellation()
            try await cap.start(filter: config.filter, width: dimensions.streamWidth, height: dimensions.streamHeight, fps: config.fps)
            try Task.checkCancellation()
            self.isRunning = true
            StreamFileLogger.shared.write(
                component: "capture",
                "started stream=\(dimensions.streamWidth)x\(dimensions.streamHeight) output=\(captureWidth)x\(captureHeight) fps=\(config.fps) crop=\(config.crop != nil)"
            )
        } catch {
            isRunning = false
            await cap.stop()
            enc.stop()
            capture = nil
            encoder = nil
            pipeWriter = nil
            throw error
        }
    }

    /// Stop capturing.
    public func stop() async {
        if isRunning {
            StreamFileLogger.shared.write(component: "capture", "stop")
        }
        isRunning = false
        await capture?.stop()
        encoder?.stop()
        capture = nil
        encoder = nil
        pipeWriter = nil
    }

    /// Update crop while running. Returns new (width, height) if encoder was reinitialized, nil if only position changed.
    public func updateCrop(_ crop: CropFilter?) -> (Int, Int)? {
        self.activeCrop = crop
        guard let crop else {
            // Removing crop — would need full restart to go back to uncropped. Skip for now.
            return nil
        }

        // If dimensions match current encoder, just a position change — no reinit needed
        let newW = crop.width
        let newH = crop.height
        guard newW != captureWidth || newH != captureHeight else { return nil }

        // Dimensions changed — reinit encoder
        encoder?.stop()
        captureWidth = newW
        captureHeight = newH

        guard let writer = pipeWriter else { return (newW, newH) }
        let enc = VideoEncoder(
            width: newW,
            height: newH,
            fps: currentFps,
            bitrateMbps: currentBitrate,
            codec: currentCodec
        ) { frameData, timestampUs in
            writer.write(type: .video, timestamp: timestampUs, data: frameData)
        }
        try? enc.start()
        self.encoder = enc

        return (newW, newH)
    }
}

struct AudioPCMConversion {
    let data: Data?
    let sampleCount: Int
    let bufferCount: Int
    let sourceChannelCount: Int
    let isInterleaved: Bool
}

enum AudioPCMConverter {
    static let outputChannelCount = 2

    static func makeStereoPCM16Data(from sampleBuffer: CMSampleBuffer, gain: Float) -> AudioPCMConversion {
        let numSamples = CMSampleBufferGetNumSamples(sampleBuffer)
        guard numSamples > 0 else {
            return AudioPCMConversion(
                data: nil,
                sampleCount: numSamples,
                bufferCount: 0,
                sourceChannelCount: 0,
                isInterleaved: false
            )
        }

        var bufferListSize = 0
        let sizeStatus = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sampleBuffer, bufferListSizeNeededOut: &bufferListSize,
            bufferListOut: nil, bufferListSize: 0,
            blockBufferAllocator: nil, blockBufferMemoryAllocator: nil,
            flags: 0, blockBufferOut: nil
        )
        guard sizeStatus == noErr, bufferListSize > 0 else {
            return AudioPCMConversion(
                data: nil,
                sampleCount: numSamples,
                bufferCount: 0,
                sourceChannelCount: 0,
                isInterleaved: false
            )
        }

        var blockBuffer: CMBlockBuffer?
        let ablMemory = UnsafeMutableRawPointer.allocate(
            byteCount: bufferListSize,
            alignment: MemoryLayout<AudioBufferList>.alignment
        )
        defer { ablMemory.deallocate() }
        let abl = ablMemory.bindMemory(to: AudioBufferList.self, capacity: 1)

        let status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sampleBuffer, bufferListSizeNeededOut: nil,
            bufferListOut: abl, bufferListSize: bufferListSize,
            blockBufferAllocator: kCFAllocatorDefault,
            blockBufferMemoryAllocator: kCFAllocatorDefault,
            flags: 0, blockBufferOut: &blockBuffer
        )
        guard status == noErr else {
            return AudioPCMConversion(
                data: nil,
                sampleCount: numSamples,
                bufferCount: 0,
                sourceChannelCount: 0,
                isInterleaved: false
            )
        }

        let bufferCount = Int(abl.pointee.mNumberBuffers)
        return withUnsafePointer(to: &abl.pointee.mBuffers) { buffersPtr in
            let buffers = UnsafeBufferPointer<AudioBuffer>(start: buffersPtr, count: bufferCount)
            let data = makeStereoPCM16Data(sampleCount: numSamples, buffers: buffers, gain: gain)
            let firstChannelCount = buffers.first.map { max(1, Int($0.mNumberChannels)) } ?? 0
            return AudioPCMConversion(
                data: data,
                sampleCount: numSamples,
                bufferCount: bufferCount,
                sourceChannelCount: firstChannelCount,
                isInterleaved: bufferCount == 1 && firstChannelCount > 1
            )
        }
    }

    static func makeStereoPCM16Data(
        sampleCount: Int,
        buffers: UnsafeBufferPointer<AudioBuffer>,
        gain: Float
    ) -> Data? {
        guard sampleCount > 0, !buffers.isEmpty else { return nil }

        let firstChannelCount = max(1, Int(buffers[0].mNumberChannels))
        let isInterleaved = buffers.count == 1 && firstChannelCount > 1

        var int16Data = Data(count: sampleCount * outputChannelCount * MemoryLayout<Int16>.size)
        int16Data.withUnsafeMutableBytes { rawBuf in
            let out = rawBuf.bindMemory(to: Int16.self)
            for sampleIndex in 0..<sampleCount {
                for channel in 0..<outputChannelCount {
                    let sample = readFloatSample(
                        sampleIndex: sampleIndex,
                        outputChannel: channel,
                        sampleCount: sampleCount,
                        buffers: buffers,
                        isInterleaved: isInterleaved
                    )
                    out[sampleIndex * outputChannelCount + channel] = pcm16(sample * gain)
                }
            }
        }
        return int16Data
    }

    private static func readFloatSample(
        sampleIndex: Int,
        outputChannel: Int,
        sampleCount: Int,
        buffers: UnsafeBufferPointer<AudioBuffer>,
        isInterleaved: Bool
    ) -> Float {
        if isInterleaved {
            let sourceChannels = max(1, Int(buffers[0].mNumberChannels))
            guard let sourceData = buffers[0].mData else { return 0 }
            let source = sourceData.bindMemory(to: Float.self, capacity: sampleCount * sourceChannels)
            return source[sampleIndex * sourceChannels + min(outputChannel, sourceChannels - 1)]
        }

        let bufferIndex = min(outputChannel, buffers.count - 1)
        guard let sourceData = buffers[bufferIndex].mData else { return 0 }
        let sourceChannels = max(1, Int(buffers[bufferIndex].mNumberChannels))
        let source = sourceData.bindMemory(to: Float.self, capacity: sampleCount * sourceChannels)
        return source[sampleIndex * sourceChannels + min(outputChannel, sourceChannels - 1)]
    }

    private static func pcm16(_ sample: Float) -> Int16 {
        guard sample.isFinite else { return 0 }
        let clamped = max(-1.0, min(1.0, sample))
        return Int16(clamped * 32767.0)
    }
}
