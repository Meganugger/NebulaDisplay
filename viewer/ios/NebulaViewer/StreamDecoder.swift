// H.264 (VideoToolbox, Annex-B) decoding to CVPixelBuffers + JPEG fallback.
// Annex-B start codes are converted to AVCC length prefixes and SPS/PPS are
// harvested from keyframes to build the format description.

import AVFoundation
import CoreImage
import Foundation
import VideoToolbox

public final class StreamDecoder {
    public var onImage: ((CGImage, _ timestampUs: UInt64) -> Void)?

    private var session: VTDecompressionSession?
    private var formatDesc: CMVideoFormatDescription?
    private let ciContext = CIContext()
    private var sawKeyframe = false

    public init() {}

    public func decode(_ frame: NdspVideoFrame) {
        switch frame.codec {
        case 0: decodeJpeg(frame)
        case 1: decodeH264(frame)
        default: break
        }
    }

    private func decodeJpeg(_ frame: NdspVideoFrame) {
        guard let provider = CGDataProvider(data: frame.payload as CFData),
              let img = CGImage(
                jpegDataProviderSource: provider, decode: nil,
                shouldInterpolate: false, intent: .defaultIntent)
        else { return }
        onImage?(img, frame.timestampUs)
    }

    // MARK: H.264

    private func decodeH264(_ frame: NdspVideoFrame) {
        let nalus = splitAnnexB(frame.payload)
        var sps: Data?
        var pps: Data?
        var vcl: [Data] = []
        for nalu in nalus {
            guard let first = nalu.first else { continue }
            switch first & 0x1F {
            case 7: sps = nalu
            case 8: pps = nalu
            case 1, 5: vcl.append(nalu)
            default: break
            }
        }
        if let sps, let pps {
            makeSession(sps: sps, pps: pps)
        }
        guard let session, let formatDesc else { return }
        if !sawKeyframe && !frame.keyframe { return }
        sawKeyframe = true

        for nalu in vcl {
            // Annex-B → AVCC (4-byte big-endian length prefix).
            var avcc = Data(count: 4)
            avcc.withUnsafeMutableBytes { $0.storeBytes(of: UInt32(nalu.count).bigEndian, as: UInt32.self) }
            avcc.append(nalu)

            var blockBuffer: CMBlockBuffer?
            let status = avcc.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> OSStatus in
                var bb: CMBlockBuffer?
                let s = CMBlockBufferCreateWithMemoryBlock(
                    allocator: kCFAllocatorDefault, memoryBlock: nil,
                    blockLength: avcc.count, blockAllocator: nil, customBlockSource: nil,
                    offsetToData: 0, dataLength: avcc.count, flags: 0, blockBufferOut: &bb)
                guard s == noErr, let bb else { return s }
                _ = CMBlockBufferReplaceDataBytes(
                    with: raw.baseAddress!, blockBuffer: bb, offsetIntoDestination: 0,
                    dataLength: avcc.count)
                blockBuffer = bb
                return noErr
            }
            guard status == noErr, let blockBuffer else { continue }

            var sampleBuffer: CMSampleBuffer?
            var sampleSize = avcc.count
            guard CMSampleBufferCreateReady(
                allocator: kCFAllocatorDefault, dataBuffer: blockBuffer,
                formatDescription: formatDesc, sampleCount: 1,
                sampleTimingEntryCount: 0, sampleTimingArray: nil,
                sampleSizeEntryCount: 1, sampleSizeArray: &sampleSize,
                sampleBufferOut: &sampleBuffer) == noErr, let sampleBuffer else { continue }

            let flags: VTDecodeFrameFlags = [._EnableAsynchronousDecompression, ._1xRealTimePlayback]
            let ts = frame.timestampUs
            VTDecompressionSessionDecodeFrame(session, sampleBuffer: sampleBuffer, flags: flags,
                                              infoFlagsOut: nil) { [weak self] _, _, imageBuffer, _, _ in
                guard let self, let imageBuffer else { return }
                let ci = CIImage(cvPixelBuffer: imageBuffer)
                if let cg = self.ciContext.createCGImage(ci, from: ci.extent) {
                    self.onImage?(cg, ts)
                }
            }
        }
    }

    private func makeSession(sps: Data, pps: Data) {
        // Rebuild only when parameter sets change.
        if session != nil, let fd = formatDesc {
            let dims = CMVideoFormatDescriptionGetDimensions(fd)
            _ = dims // parameter-set change detection below covers resolution changes
        }
        var fd: CMVideoFormatDescription?
        let status: OSStatus = sps.withUnsafeBytes { spsPtr in
            pps.withUnsafeBytes { ppsPtr in
                var params: [UnsafePointer<UInt8>] = [
                    spsPtr.bindMemory(to: UInt8.self).baseAddress!,
                    ppsPtr.bindMemory(to: UInt8.self).baseAddress!,
                ]
                var sizes = [sps.count, pps.count]
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault, parameterSetCount: 2,
                    parameterSetPointers: &params, parameterSetSizes: &sizes,
                    nalUnitHeaderLength: 4, formatDescriptionOut: &fd)
            }
        }
        guard status == noErr, let fd else { return }
        if let old = formatDesc, CMFormatDescriptionEqual(old, otherFormatDescription: fd) { return }

        if let s = session { VTDecompressionSessionInvalidate(s) }
        formatDesc = fd
        var newSession: VTDecompressionSession?
        let attrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
        ]
        VTDecompressionSessionCreate(
            allocator: kCFAllocatorDefault, formatDescription: fd,
            decoderSpecification: nil, imageBufferAttributes: attrs as CFDictionary,
            outputCallback: nil, decompressionSessionOut: &newSession)
        session = newSession
        sawKeyframe = false
    }

    private func splitAnnexB(_ data: Data) -> [Data] {
        var out: [Data] = []
        var i = data.startIndex
        var start: Int? = nil
        while i < data.endIndex - 2 {
            if data[i] == 0, data[i + 1] == 0, data[i + 2] == 1 {
                let codeStart = (i > data.startIndex && data[i - 1] == 0) ? i - 1 : i
                if let s = start { out.append(data.subdata(in: s..<codeStart)) }
                start = i + 3
                i += 3
            } else {
                i += 1
            }
        }
        if let s = start { out.append(data.subdata(in: s..<data.endIndex)) }
        return out
    }

    public func invalidate() {
        if let s = session { VTDecompressionSessionInvalidate(s) }
        session = nil
        formatDesc = nil
    }
}
