import AVFoundation
import CoreMedia
import Foundation
import os
import UIKit
import VideoToolbox

/// Runtime playback capabilities for this Apple device, sent to `/decision` so
/// the server only transcodes what AVPlayer/VideoToolbox genuinely can't take.
/// Apple's shape differs from Android's: AVPlayer direct-plays MP4/MOV/M4V (not
/// MKV/TS), plays AAC/AC3/E-AC3 (never DTS/TrueHD), and HEVC/AV1 ride hardware
/// decode where present — so MKV or DTS files come back as HLS instead.
enum Caps {
    private static let logger = Logger(subsystem: "tv.plurx.app", category: "capabilities")

    static func query() -> [URLQueryItem] {
        let hevc = VTIsHardwareDecodeSupported(kCMVideoCodecType_HEVC)
        let av1 = VTIsHardwareDecodeSupported(kCMVideoCodecType_AV1)
        var result = query(
            hevc: hevc,
            av1: av1,
            displayHDR: displayIsHDR,
            dolbyVision: dolbyVisionIsAvailable
        )
        result.append(URLQueryItem(name: "client", value: "apple"))
        result.append(URLQueryItem(name: "device", value: UIDevice.current.model))
        let snapshot = result.map { "\($0.name)=\($0.value ?? "")" }.joined(separator: " ")
        logger.info("runtime playback capabilities: \(snapshot, privacy: .public)")
        return result
    }

    /// Pure spelling of the wire capabilities. Keeping AVFoundation outside
    /// this half makes the important distinction regression-testable: a
    /// display may support HDR10 without supporting Dolby Vision.
    static func query(
        hevc: Bool,
        av1: Bool,
        displayHDR: Bool,
        dolbyVision: Bool
    ) -> [URLQueryItem] {
        var vcodec = ["h264"]
        if hevc { vcodec.append("hevc") }
        if av1 { vcodec.append("av1") }
        // AVPlayer handles these audio codecs; DTS / TrueHD are deliberately out.
        let acodec = ["aac", "ac3", "eac3", "alac", "mp3"]
        // Containers AVPlayer will direct-play from a progressive URL. Audio
        // containers matter here too: omitting M4B made a perfectly playable
        // audiobook enter the video-oriented HLS copy path and fail before
        // its first frame-equivalent audio sample on physical devices.
        let container = ["mp4", "mov", "m4v", "m4a", "m4b", "mp3", "aac", "flac", "wav"]
        let supportsDolbyVision = hevc && displayHDR && dolbyVision

        return [
            URLQueryItem(name: "vcodec", value: vcodec.joined(separator: ",")),
            URLQueryItem(name: "acodec", value: acodec.joined(separator: ",")),
            URLQueryItem(name: "container", value: container.joined(separator: ",")),
            URLQueryItem(name: "hdr", value: displayHDR ? "1" : "0"),
            // Generic HDR eligibility must not be promoted into a Dolby
            // Vision claim. On an HDR10-only output that overclaim makes the
            // server preserve DV metadata, AVPlayer rejects the stream, and
            // the compatibility retry needlessly tone-maps it all the way to
            // SDR. A conservative DV=0 lets the server expose the untouched
            // HDR10-compatible base instead.
            // Profile 5 and 8 are advertised explicitly so the server never
            // mistakes support for those delivery profiles as support for
            // Blu-ray Profile 7.
            URLQueryItem(name: "dv", value: supportsDolbyVision ? "1" : "0"),
            URLQueryItem(
                name: "dvprofile",
                value: supportsDolbyVision ? "5,8" : ""
            ),
            // AVPlayer's decoder accepts P5/P8 but raw progressive MP4 is not
            // a reliable DV delivery envelope: it can advance with audio and
            // report Dolby Vision while rendering black. Ask the server for a
            // copy-video HLS remux whenever it preserves DV. No samples are
            // re-encoded; the signaling/transport alone is normalized.
            URLQueryItem(name: "dvhls", value: "1"),
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

    /// The format-specific signal is the only public API that can distinguish
    /// a Dolby Vision output from an HDR10-only one. Apple deprecated it in
    /// iOS/tvOS 26 in favor of generic HDR eligibility, but that replacement
    /// deliberately does not name a format. The old property remains present
    /// and display-aware on 26, so use its DV bit as a compatibility signal and
    /// still require `displayIsHDR` in `query` before making the wire claim.
    ///
    /// Do not replace this with `eligibleForHDRPlayback` alone: that promotes
    /// an HDR10-only HDMI path to Dolby Vision and can hand AVPlayer a stream
    /// its current display cannot present.
    private static var dolbyVisionIsAvailable: Bool {
        return AVPlayer.availableHDRModes.contains(.dolbyVision)
    }
}
