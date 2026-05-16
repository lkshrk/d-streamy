import Foundation
import CoreMedia
import AVFoundation
import ScreenCaptureKit

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
        self.outputFd = outputFd
    }
}

/// Manages a window capture + encode + pipe session.
public final class CaptureSession {
    private var capture: WindowCapture?
    private var encoder: VideoEncoder?
    private var pipeWriter: PipeWriter?

    public private(set) var captureWidth: Int = 0
    public private(set) var captureHeight: Int = 0
    public private(set) var isRunning = false

    public init() {}

    /// Start capturing. Call from an async context.
    public func start(config: CaptureConfig) async throws {
        let srcW = config.sourceWidth
        let srcH = config.sourceHeight

        // Compute dimensions preserving aspect ratio
        if let crop = config.crop {
            captureWidth = crop.width
            captureHeight = crop.height
        } else if srcW > 0 && srcH > 0 {
            let scaleW = Double(config.maxWidth) / Double(srcW)
            let scaleH = Double(config.maxHeight) / Double(srcH)
            let scale = min(scaleW, scaleH, 1.0)
            captureWidth = max(2, Int(Double(srcW) * scale) & ~1)
            captureHeight = max(2, Int(Double(srcH) * scale) & ~1)
        } else {
            captureWidth = config.maxWidth
            captureHeight = config.maxHeight
        }

        let writer = PipeWriter(fd: config.outputFd)
        self.pipeWriter = writer

        let enc = VideoEncoder(
            width: captureWidth,
            height: captureHeight,
            fps: config.fps,
            bitrateMbps: config.bitrateMbps
        ) { frameData, timestampUs in
            writer.write(type: .video, timestamp: timestampUs, data: frameData)
        }
        try enc.start()
        self.encoder = enc

        let cap = WindowCapture()

        // Video
        cap.onVideoFrame = { [weak self] pixelBuffer, pts in
            guard let self, let encoder = self.encoder else { return }
            let buf: CVPixelBuffer
            if let crop = config.crop {
                guard let cropped = crop.apply(to: pixelBuffer) else { return }
                buf = cropped
            } else {
                buf = pixelBuffer
            }
            encoder.encode(pixelBuffer: buf, presentationTime: pts)
        }

        // Audio
        let audioGain = config.audioGain
        cap.onAudioBuffer = { sampleBuffer in
            let pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
            guard pts.isValid, !pts.seconds.isNaN, !pts.seconds.isInfinite else { return }
            let timestampUs = UInt64(max(0, pts.seconds * 1_000_000))

            let numSamples = CMSampleBufferGetNumSamples(sampleBuffer)
            guard numSamples > 0 else { return }

            var bufferListSize = 0
            CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
                sampleBuffer, bufferListSizeNeededOut: &bufferListSize,
                bufferListOut: nil, bufferListSize: 0,
                blockBufferAllocator: nil, blockBufferMemoryAllocator: nil,
                flags: 0, blockBufferOut: nil
            )

            var blockBuffer: CMBlockBuffer?
            let ablMemory = UnsafeMutableRawPointer.allocate(byteCount: bufferListSize, alignment: MemoryLayout<AudioBufferList>.alignment)
            defer { ablMemory.deallocate() }
            let abl = ablMemory.bindMemory(to: AudioBufferList.self, capacity: 1)

            let status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
                sampleBuffer, bufferListSizeNeededOut: nil,
                bufferListOut: abl, bufferListSize: bufferListSize,
                blockBufferAllocator: kCFAllocatorDefault,
                blockBufferMemoryAllocator: kCFAllocatorDefault,
                flags: 0, blockBufferOut: &blockBuffer
            )
            guard status == noErr else { return }

            let bufferCount = Int(abl.pointee.mNumberBuffers)
            let channels = min(bufferCount, 2)
            var int16Data = Data(count: numSamples * channels * MemoryLayout<Int16>.size)
            int16Data.withUnsafeMutableBytes { rawBuf in
                let out = rawBuf.bindMemory(to: Int16.self)
                withUnsafePointer(to: &abl.pointee.mBuffers) { buffersPtr in
                    let buffers = UnsafeBufferPointer<AudioBuffer>(start: buffersPtr, count: bufferCount)
                    for ch in 0..<channels {
                        guard let srcRaw = buffers[ch].mData else { continue }
                        let src = srcRaw.bindMemory(to: Float.self, capacity: numSamples)
                        for i in 0..<numSamples {
                            let sample = max(-1.0, min(1.0, src[i] * audioGain))
                            out[i * channels + ch] = Int16(sample * 32767.0)
                        }
                    }
                }
            }
            writer.write(type: .audio, timestamp: timestampUs, data: int16Data)
        }

        try await cap.start(filter: config.filter, width: captureWidth, height: captureHeight, fps: config.fps)
        self.capture = cap
        self.isRunning = true
    }

    /// Stop capturing.
    public func stop() async {
        isRunning = false
        await capture?.stop()
        encoder?.stop()
        capture = nil
        encoder = nil
        pipeWriter = nil
    }
}
