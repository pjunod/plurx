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
        let hevc = VTIsHardwareDecodeSupported(kCMVideoCodecType_HEVC)
        if hevc { vcodec.append("hevc") }
        if VTIsHardwareDecodeSupported(kCMVideoCodecType_AV1) { vcodec.append("av1") }
        // AVPlayer handles these audio codecs; DTS / TrueHD are deliberately out.
        let acodec = ["aac", "ac3", "eac3", "alac", "mp3"]
        // Containers AVPlayer will direct-play from a progressive URL.
        let container = ["mp4", "mov", "m4v"]

        return [
            URLQueryItem(name: "vcodec", value: vcodec.joined(separator: ",")),
            URLQueryItem(name: "acodec", value: acodec.joined(separator: ",")),
            URLQueryItem(name: "container", value: container.joined(separator: ",")),
            URLQueryItem(name: "hdr", value: displayIsHDR ? "1" : "0"),
            // The former profile-specific HDR API was removed in iOS/tvOS 26.
            // Apple's replacement is the display-aware eligibility signal;
            // with hardware HEVC it is the supported
            // public capability check for AVPlayer's HDR/Dolby Vision path.
            // Profile 5 and 8 are advertised explicitly so the server never
            // mistakes support for those delivery profiles as support for
            // Blu-ray Profile 7.
            URLQueryItem(name: "dv", value: dolbyVisionSupported(hevc: hevc) ? "1" : "0"),
            URLQueryItem(
                name: "dvprofile",
                value: dolbyVisionSupported(hevc: hevc) ? "5,8" : ""
            ),
        ]
    }

    /// True when this device can present HDR to its current display — mirrors
    /// the server's tone-map-on-SDR rule.
    ///
    /// This is also the *whole* of what the dynamic-range badge is allowed to
    /// know about rendering (MEDIA-BADGES-PLAN.md §2.2): AVFoundation exposes
    /// nothing public about which HLS variant is active, and EDR-headroom
    /// polling is a stated non-goal. Read live rather than cached — an Apple TV
    /// whose output format changes in Settings changes this answer without the
    /// app relaunching.
    static var displayIsHDR: Bool {
        AVPlayer.eligibleForHDRPlayback
    }

    private static func dolbyVisionSupported(hevc: Bool) -> Bool {
        hevc && displayIsHDR
    }
}
