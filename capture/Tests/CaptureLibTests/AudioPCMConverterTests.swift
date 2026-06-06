import AVFoundation
@testable import CaptureLib
import XCTest

final class AudioPCMConverterTests: XCTestCase {
    func testInterleavedStereoFloat32BecomesStereoPCM16() {
        var samples: [Float] = [
            0.25, -0.25,
            0.50, -0.50,
        ]
        let byteCount = UInt32(samples.count * MemoryLayout<Float>.size)

        let pcm = samples.withUnsafeMutableBufferPointer { samplePtr in
            var audioBuffer = AudioBuffer(
                mNumberChannels: 2,
                mDataByteSize: byteCount,
                mData: UnsafeMutableRawPointer(samplePtr.baseAddress!)
            )
            return withUnsafePointer(to: &audioBuffer) { bufferPtr in
                let buffers = UnsafeBufferPointer<AudioBuffer>(start: bufferPtr, count: 1)
                return AudioPCMConverter.makeStereoPCM16Data(sampleCount: 2, buffers: buffers, gain: 1.0)
            }
        }

        XCTAssertEqual(pcm?.int16Samples(), [8191, -8191, 16383, -16383])
    }

    func testPlanarMonoFloat32DuplicatesToStereoPCM16() {
        var samples: [Float] = [0.25, -0.50]
        let byteCount = UInt32(samples.count * MemoryLayout<Float>.size)

        let pcm = samples.withUnsafeMutableBufferPointer { samplePtr in
            var audioBuffer = AudioBuffer(
                mNumberChannels: 1,
                mDataByteSize: byteCount,
                mData: UnsafeMutableRawPointer(samplePtr.baseAddress!)
            )
            return withUnsafePointer(to: &audioBuffer) { bufferPtr in
                let buffers = UnsafeBufferPointer<AudioBuffer>(start: bufferPtr, count: 1)
                return AudioPCMConverter.makeStereoPCM16Data(sampleCount: 2, buffers: buffers, gain: 1.0)
            }
        }

        XCTAssertEqual(pcm?.int16Samples(), [8191, 8191, -16383, -16383])
    }

    func testPlanarStereoFloat32BecomesStereoPCM16() {
        var left: [Float] = [1.0, -1.0]
        var right: [Float] = [0.0, 0.5]
        let leftByteCount = UInt32(left.count * MemoryLayout<Float>.size)
        let rightByteCount = UInt32(right.count * MemoryLayout<Float>.size)

        let pcm = left.withUnsafeMutableBufferPointer { leftPtr in
            right.withUnsafeMutableBufferPointer { rightPtr in
                let audioBuffers = [
                    AudioBuffer(
                        mNumberChannels: 1,
                        mDataByteSize: leftByteCount,
                        mData: UnsafeMutableRawPointer(leftPtr.baseAddress!)
                    ),
                    AudioBuffer(
                        mNumberChannels: 1,
                        mDataByteSize: rightByteCount,
                        mData: UnsafeMutableRawPointer(rightPtr.baseAddress!)
                    ),
                ]
                return audioBuffers.withUnsafeBufferPointer { bufferPtr in
                    AudioPCMConverter.makeStereoPCM16Data(sampleCount: 2, buffers: bufferPtr, gain: 1.0)
                }
            }
        }

        XCTAssertEqual(pcm?.int16Samples(), [32767, 0, -32767, 16383])
    }
}

private extension Data {
    func int16Samples() -> [Int16] {
        withUnsafeBytes { rawBuffer in
            Array(rawBuffer.bindMemory(to: Int16.self))
        }
    }
}
