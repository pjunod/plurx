import AVFoundation
import CoreMedia
import Foundation
import VideoToolbox

/// Runtime playback capabilities for this Apple device, sent to `/decision` so
/// the server only transcodes what AVPlayer/VideoToolbox genuinely can't take.
/// Apple's shape differs from Android's: AVPlayer direct-plays MP4/MOV/M4V (not
/// MKV/TS), plays AAC/AC3/E-AC3 (never DTS/TrueHD), and HEVC/AV1 ride hardware
/// decode where present — so MKV or DTS files come back as HLS instead.
enum Caps {
    static func query() -> [URLQueryItem] {
        var vcodec = ["h264"]
        if VTIsHardwareDecodeSupported(kCMVideoCodecType_HEVC) { vcodec.append("hevc") }
        if VTIsHardwareDecodeSupported(kCMVideoCodecType_AV1) { vcodec.append("av1") }
        // AVPlayer handles these audio codecs; DTS / TrueHD are deliberately out.
        let acodec = ["aac", "ac3", "eac3", "alac", "mp3"]
        // Containers AVPlayer will direct-play from a progressive URL.
        let container = ["mp4", "mov", "m4v"]

        return [
            URLQueryItem(name: "vcodec", value: vcodec.joined(separator: ",")),
            URLQueryItem(name: "acodec", value: acodec.joined(separator: ",")),
            URLQueryItem(name: "container", value: container.joined(separator: ",")),
            URLQueryItem(name: "hdr", value: hdrSupported ? "1" : "0"),
        ]
    }

    /// True when the current display advertises any HDR mode — mirrors the
    /// server's tone-map-on-SDR rule.
    private static var hdrSupported: Bool {
        !AVPlayer.availableHDRModes.isEmpty
    }
}
