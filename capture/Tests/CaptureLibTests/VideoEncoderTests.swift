import XCTest
import CoreMedia
import CoreVideo
@testable import CaptureLib

final class VideoEncoderTests: XCTestCase {
    private func makePixelBuffer(width: Int, height: Int) -> CVPixelBuffer {
        var pb: CVPixelBuffer?
        let attrs: CFDictionary = [
            kCVPixelBufferCGImageCompatibilityKey: true,
            kCVPixelBufferCGBitmapContextCompatibilityKey: true,
        ] as CFDictionary
        let status = CVPixelBufferCreate(
            kCFAllocatorDefault, width, height, kCVPixelFormatType_32BGRA, attrs, &pb
        )
        guard status == kCVReturnSuccess, let buffer = pb else {
            fatalError("CVPixelBufferCreate failed: \(status)")
        }
        CVPixelBufferLockBaseAddress(buffer, [])
        if let base = CVPixelBufferGetBaseAddress(buffer) {
            memset(base, 0x7F, CVPixelBufferGetBytesPerRow(buffer) * height)
        }
        CVPixelBufferUnlockBaseAddress(buffer, [])
        return buffer
    }

    /// Annex-B NAL unit types present in a frame (HEVC: (byte>>1)&0x3F, H264: byte&0x1F).
    private func nalTypes(_ data: [UInt8], hevc: Bool) -> Set<Int> {
        var types = Set<Int>()
        var i = 0
        while i + 4 <= data.count {
            if data[i] == 0, data[i + 1] == 0, data[i + 2] == 0, data[i + 3] == 1 {
                let h = i + 4
                if h < data.count {
                    let b = data[h]
                    types.insert(hevc ? Int((b >> 1) & 0x3F) : Int(b & 0x1F))
                }
                i += 4
            } else {
                i += 1
            }
        }
        return types
    }

    private func firstKeyframe(codec: VideoCodec) throws -> [UInt8] {
        let frames = NSMutableArray()
        let lock = NSLock()
        let exp = expectation(description: "encoded frame")
        exp.assertForOverFulfill = false

        let enc = VideoEncoder(width: 320, height: 240, fps: 30, bitrateMbps: 2.0, codec: codec) { data, _ in
            lock.lock()
            frames.add(data)
            lock.unlock()
            exp.fulfill()
        }

        do {
            try enc.start()
        } catch {
            throw XCTSkip("Encoder unavailable for \(codec.rawValue): \(error)")
        }

        let pb = makePixelBuffer(width: 320, height: 240)
        enc.forceKeyframe(pixelBuffer: pb, presentationTime: CMTime(value: 0, timescale: 600))
        enc.encode(pixelBuffer: pb, presentationTime: CMTime(value: 20, timescale: 600))
        wait(for: [exp], timeout: 5)
        enc.stop()

        lock.lock()
        defer { lock.unlock() }
        guard let first = frames.firstObject as? Data else {
            throw XCTSkip("No frame produced")
        }
        return [UInt8](first)
    }

    func testH264KeyframeHasParameterSets() throws {
        let types = nalTypes(try firstKeyframe(codec: .h264), hevc: false)
        XCTAssertTrue(types.contains(7), "missing SPS (got \(types.sorted()))")
        XCTAssertTrue(types.contains(8), "missing PPS (got \(types.sorted()))")
        XCTAssertTrue(types.contains(5), "missing IDR slice (got \(types.sorted()))")
    }

    func testH265KeyframeHasParameterSets() throws {
        let types = nalTypes(try firstKeyframe(codec: .h265), hevc: true)
        XCTAssertTrue(types.contains(32), "missing VPS (got \(types.sorted()))")
        XCTAssertTrue(types.contains(33), "missing SPS (got \(types.sorted()))")
        XCTAssertTrue(types.contains(34), "missing PPS (got \(types.sorted()))")
    }
}
