import SwiftUI

/// The detail screen's answer to "does this have my audio and subtitles?", and
/// the pre-play choice that follows from it.
///
/// Everything here renders the server's `audio_streams`, `subtitle_streams`,
/// and `playback_defaults` (docs/CLIENTS.md §1, "Shared track facts"). None of
/// it reimplements `select_tracks`, reads admin settings, or guesses which
/// track policy would pick: the server already answered, and the point of the
/// shared contract is that every platform tells the same story about the same
/// file.
enum TrackFacts {

    // MARK: - Rows

    /// One track as the detail screen lists it.
    struct Row: Identifiable, Equatable {
        /// Stream index, or `PrePlaySelection.subtitleOff` for the Off row.
        let index: Int
        /// "English · Dolby Digital Plus · 5.1" — language first, because the
        /// question this screen answers is a question about language.
        let label: String
        /// "Forced", "SDH", or both. Empty when the track claims neither.
        let markers: [String]
        /// Marked by `playback_defaults`, not by the container's own flag.
        let isServerDefault: Bool

        var id: Int { index }

        /// Everything on one line, for a menu label and for VoiceOver.
        var summary: String {
            ([label] + markers).joined(separator: " · ")
        }
    }

    static func audioRows(_ file: MediaFile) -> [Row] {
        let defaultIndex = file.playbackDefaults?.audio?.selectedIndex
        return (file.audioStreams ?? []).map { track in
            Row(
                index: track.index,
                label: audioLabel(track),
                markers: [],
                isServerDefault: track.index == defaultIndex
            )
        }
    }

    static func subtitleRows(_ file: MediaFile) -> [Row] {
        let defaultIndex = file.playbackDefaults?.subtitle?.selectedIndex
        return (file.subtitleStreams ?? []).map { track in
            Row(
                index: track.index,
                label: subtitleLabel(track),
                markers: subtitleMarkers(track),
                isServerDefault: track.index == defaultIndex
            )
        }
    }

    /// What the Subtitles control shows when nothing is selected. `no_tracks`
    /// is a fact about the file; Off is a choice about this playback, and a
    /// file with no subtitles offers no choice at all.
    static let noSubtitleTracksLine = "This file has no subtitle tracks."
    static let noAudioTracksLine = "This file has no audio tracks."

    // MARK: - Labels

    static func audioLabel(_ track: AudioTrack) -> String {
        var parts = [languageName(track.language)]
        parts.append(codecName(track.codec))
        if let channels = track.channels, let layout = channelLayout(channels) {
            parts.append(layout)
        }
        if let title = trimmed(track.title) { parts.append(title) }
        return parts.joined(separator: " · ")
    }

    static func subtitleLabel(_ track: SubtitleStream) -> String {
        var parts = [languageName(track.language)]
        parts.append(subtitleFormatName(track.codec))
        if let title = trimmed(track.title) { parts.append(title) }
        return parts.joined(separator: " · ")
    }

    /// Forced and SDH are separate claims and a track can make both. SDH falls
    /// back to the title when the container carries no `hearing_impaired`
    /// disposition, using the server's own marker list — see
    /// `sdhTitleMarkers`.
    static func subtitleMarkers(_ track: SubtitleStream) -> [String] {
        var markers: [String] = []
        if track.forced || titleMarksForced(track.title) { markers.append("Forced") }
        if track.hearingImpaired == true || titleMarksSDH(track.title) { markers.append("SDH") }
        return markers
    }

    /// A display language name, falling back to the raw tag when the system
    /// cannot name it. `und` and an absent tag are the same thing to a viewer:
    /// nobody wrote down what language this is.
    static func languageName(_ code: String?) -> String {
        guard let code = trimmed(code), code.lowercased() != "und" else {
            return "Untagged"
        }
        let locale = Locale(identifier: "en")
        if let named = locale.localizedString(forLanguageCode: code), !named.isEmpty {
            return named.prefix(1).uppercased() + named.dropFirst()
        }
        return code.uppercased()
    }

    static func codecName(_ codec: String) -> String {
        switch codec.lowercased() {
        case "aac": return "AAC"
        case "ac3": return "Dolby Digital"
        case "eac3": return "Dolby Digital Plus"
        case "truehd": return "Dolby TrueHD"
        case "dts": return "DTS"
        case "flac": return "FLAC"
        case "opus": return "Opus"
        case "mp3": return "MP3"
        case "pcm_s16le", "pcm_s24le", "pcm_s32le": return "LPCM"
        case "vorbis": return "Vorbis"
        default: return codec.uppercased()
        }
    }

    static func subtitleFormatName(_ codec: String) -> String {
        switch codec.lowercased() {
        case "subrip", "srt": return "SubRip"
        case "webvtt", "vtt": return "WebVTT"
        case "ass", "ssa": return "ASS"
        case "mov_text": return "Timed text"
        case "hdmv_pgs_subtitle", "pgssub", "pgs": return "PGS"
        case "dvd_subtitle", "dvdsub": return "VobSub"
        case "dvb_subtitle": return "DVB"
        default: return codec.uppercased()
        }
    }

    static func channelLayout(_ channels: Int) -> String? {
        switch channels {
        case ..<1: return nil
        case 1: return "Mono"
        case 2: return "Stereo"
        case 6: return "5.1"
        case 8: return "7.1"
        default: return "\(channels).0"
        }
    }

    // MARK: - Preferred-language status

    /// The plain sentence for one `preferred_language_status`, rendered for all
    /// five states so a viewer learns "English audio, no English subtitles"
    /// without starting playback.
    ///
    /// `unknown` is worded as *can't tell* and never as *missing*: an untagged
    /// track means absence cannot be claimed, and saying "no English subtitles"
    /// there would be a claim the server explicitly declined to make.
    static func statusLine(
        _ preference: PlaybackTrackDefault?,
        isSubtitle: Bool
    ) -> String? {
        guard let preference, let status = preference.preferredLanguageStatus else { return nil }
        let language = languageName(preference.preferredLanguage)
        let kind = isSubtitle ? "subtitles" : "audio"
        switch status {
        case .selected:
            return isSubtitle
                ? "\(language) subtitles are on."
                : "\(language) audio is selected."
        case .available:
            return isSubtitle
                ? "\(language) subtitles are here, but not switched on."
                : "\(language) audio is here, but another track is selected."
        case .missing:
            return "No \(language) \(kind)."
        case .unknown:
            return "Can't tell whether there are \(language) \(kind) — "
                + "some tracks carry no language tag."
        case .noTracks:
            return isSubtitle ? noSubtitleTracksLine : noAudioTracksLine
        }
    }

    // MARK: - Control summaries

    /// What the Audio control reads before it is opened.
    static func audioSummary(_ file: MediaFile, chosen: Int?) -> String {
        let rows = audioRows(file)
        guard !rows.isEmpty else { return "None" }
        let index = chosen ?? file.playbackDefaults?.audio?.selectedIndex
        guard let row = rows.first(where: { $0.index == index }) else { return "Default" }
        return row.summary
    }

    /// What the Subtitles control reads before it is opened. Off is a real
    /// answer here, both as the server's default and as the viewer's choice.
    static func subtitleSummary(_ file: MediaFile, chosen: Int?) -> String {
        let rows = subtitleRows(file)
        guard !rows.isEmpty else { return "None" }
        if chosen == PrePlaySelection.subtitleOff { return "Off" }
        let index = chosen ?? file.playbackDefaults?.subtitle?.selectedIndex
        guard let index, let row = rows.first(where: { $0.index == index }) else { return "Off" }
        return row.summary
    }

    /// The burn-in cost of a pre-play subtitle choice, disclosed before
    /// playback starts.
    ///
    /// This is the detail screen's copy of the *presentation* rule, not of the
    /// policy: bitmap tracks with no overlay route have to be drawn into the
    /// video, and styled ASS/SSA and `mov_text` cannot be published as WebVTT
    /// renditions even though they contain text. `/decision` remains the
    /// authority once playback starts — it answers
    /// `selection.subtitle_requires_burn_in` — but a warning that only appears
    /// after playback begins is not a warning made before pressing play.
    static func burnInWarning(_ file: MediaFile, chosen: Int?) -> String? {
        guard let chosen, chosen != PrePlaySelection.subtitleOff else { return nil }
        guard let track = (file.subtitleStreams ?? []).first(where: { $0.index == chosen })
        else { return nil }
        guard requiresBurnIn(track) else { return nil }
        // PGS is the one bitmap format with an escape: a server with the
        // `pgs-v1` overlay enabled draws it in the app instead. That flag rides
        // on `/decision`'s tracks, which a detail screen has not fetched, so
        // this says "unless" rather than claiming a burn the server may avoid.
        if isPGS(track) {
            return "Unless this server draws PGS subtitles as an overlay, this "
                + "track has to be burned into the picture — and playback will "
                + "re-encode the video instead of streaming it untouched."
        }
        return "This track has to be drawn into the picture, so playback will "
            + "re-encode the video instead of streaming it untouched."
    }

    static func isPGS(_ track: SubtitleStream) -> Bool {
        ["hdmv_pgs_subtitle", "pgssub", "pgs"].contains(track.codec.lowercased())
    }

    /// Formats that cannot ride along as a selectable track.
    static func requiresBurnIn(_ track: SubtitleStream) -> Bool {
        switch track.codec.lowercased() {
        case "subrip", "srt", "webvtt", "vtt": return false
        default: return true
        }
    }

    // MARK: - Selection lifecycle

    /// A pre-play choice belongs to one file. Moving to another item — or to
    /// another part of an audiobook — starts from the server's defaults again,
    /// which is what "per playback" means.
    static func selection(
        _ current: PrePlaySelection,
        carriedFrom previousFileId: Int?,
        to fileId: Int?
    ) -> PrePlaySelection {
        previousFileId == fileId ? current : .none
    }

    // MARK: - Title sniffing

    private static func titleMarksForced(_ title: String?) -> Bool {
        guard let title = trimmed(title) else { return false }
        return PlayerController.titleMarksForced(title)
    }

    /// The server's own SDH title fallback, mirrored exactly.
    ///
    /// `subtitle_characteristics` (crates/plurxd/src/http/hls.rs) decides which
    /// tracks get the accessibility characteristics on an HLS rendition. If the
    /// detail screen sniffed a different list, the same track would read as SDH
    /// in one place and not the other — two answers to one question, which is
    /// what the shared contract exists to prevent. Grow this list only when the
    /// server grows its own.
    ///
    /// Notably absent: a bare `cc`. It is a substring of ordinary words —
    /// "Tracce", "Piccadilly", "Soccer Commentary" — and the same over-eager
    /// match `titleMarksForced` already warns about. `closed caption` carries
    /// that meaning without the false positives.
    private static let sdhTitleMarkers = [
        "sdh",
        "closed caption",
        "closed-caption",
        "hard of hearing",
        "non udenti",
    ]

    private static func titleMarksSDH(_ title: String?) -> Bool {
        guard let title = trimmed(title)?.lowercased() else { return false }
        return sdhTitleMarkers.contains(where: title.contains)
    }

    private static func trimmed(_ value: String?) -> String? {
        let text = value?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return text.isEmpty ? nil : text
    }
}

// MARK: - Views

/// The detail screen's track list: every audio and subtitle track the file
/// carries, the server-selected default marked in each list, and the preferred
/// language status underneath.
struct TrackFactsSection: View {
    let file: MediaFile

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            trackList(
                title: "AUDIO",
                rows: TrackFacts.audioRows(file),
                emptyLine: TrackFacts.noAudioTracksLine,
                status: TrackFacts.statusLine(file.playbackDefaults?.audio, isSubtitle: false)
            )
            trackList(
                title: "SUBTITLES",
                rows: TrackFacts.subtitleRows(file),
                emptyLine: TrackFacts.noSubtitleTracksLine,
                status: TrackFacts.statusLine(file.playbackDefaults?.subtitle, isSubtitle: true)
            )
        }
        .frame(maxWidth: EpisodeMediaInfoMetrics.maximumWidth, alignment: .leading)
    }

    @ViewBuilder
    private func trackList(
        title: String,
        rows: [TrackFacts.Row],
        emptyLine: String,
        status: String?
    ) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(title)
                .font(.system(size: 10, weight: .bold, design: .rounded))
                .tracking(1.5)
                .foregroundStyle(Palette.accent.opacity(0.9))

            VStack(alignment: .leading, spacing: 0) {
                if rows.isEmpty {
                    // Never an empty box: "no subtitles" is the answer the
                    // viewer came for just as often as a track list is.
                    Text(emptyLine)
                        .font(.system(.caption, design: .rounded))
                        .foregroundStyle(Palette.onBg.opacity(0.82))
                        .padding(.vertical, 8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    ForEach(Array(rows.enumerated()), id: \.element.id) { position, row in
                        if position > 0 {
                            Divider().overlay(Palette.outline.opacity(0.5))
                        }
                        trackRow(row)
                    }
                }
            }
            .padding(.horizontal, 11)
            .background(
                Palette.surfaceHi.opacity(0.66),
                in: RoundedRectangle(cornerRadius: 12, style: .continuous)
            )
            .overlay {
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .stroke(Palette.outline.opacity(0.72), lineWidth: 0.75)
            }

            if let status {
                Text(status)
                    .font(.system(.caption, design: .rounded))
                    .foregroundStyle(Palette.muted)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func trackRow(_ row: TrackFacts.Row) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: row.isServerDefault ? "checkmark.circle.fill" : "circle")
                .font(.system(size: 11))
                .foregroundStyle(row.isServerDefault ? Palette.accent : Palette.outline)
                .accessibilityHidden(true)
            Text(row.label)
                .font(.system(.caption, design: .rounded))
                .foregroundStyle(Palette.onBg.opacity(0.82))
                .fixedSize(horizontal: false, vertical: true)
            ForEach(row.markers, id: \.self) { marker in
                Text(marker)
                    .font(.system(size: 9, weight: .bold, design: .rounded))
                    .tracking(0.6)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 2)
                    .background(
                        Palette.outline.opacity(0.5),
                        in: Capsule()
                    )
                    .foregroundStyle(Palette.onBg.opacity(0.86))
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 8)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(
            row.isServerDefault ? "\(row.summary), selected by default" : row.summary
        )
    }
}

/// The pre-play choice itself: two menus next to Play. Kept beside the play
/// action rather than only in the list below, because criterion 3 is about
/// choosing *before* pressing play.
struct TrackChoiceControls: View {
    let file: MediaFile
    @Binding var selection: PrePlaySelection

    var body: some View {
        let audioRows = TrackFacts.audioRows(file)
        let subtitleRows = TrackFacts.subtitleRows(file)
        Group {
            if audioRows.count > 1 {
                choiceMenu(
                    title: "Audio",
                    symbol: "speaker.wave.2.fill",
                    value: TrackFacts.audioSummary(file, chosen: selection.audioIndex)
                ) {
                    ForEach(audioRows) { row in
                        Button {
                            selection.audioIndex = row.index
                        } label: {
                            Label(
                                menuLabel(row),
                                systemImage: isChosenAudio(row) ? "checkmark" : "speaker.wave.2"
                            )
                        }
                    }
                }
            }
            if !subtitleRows.isEmpty {
                choiceMenu(
                    title: "Subtitles",
                    symbol: "captions.bubble.fill",
                    value: TrackFacts.subtitleSummary(file, chosen: selection.subtitleIndex)
                ) {
                    Button {
                        selection.subtitleIndex = PrePlaySelection.subtitleOff
                    } label: {
                        Label(
                            "Off",
                            systemImage: isChosenSubtitleOff ? "checkmark" : "captions.bubble"
                        )
                    }
                    ForEach(subtitleRows) { row in
                        Button {
                            selection.subtitleIndex = row.index
                        } label: {
                            Label(
                                menuLabel(row),
                                systemImage: isChosenSubtitle(row) ? "checkmark" : "captions.bubble"
                            )
                        }
                    }
                }
            }
        }
    }

    private func menuLabel(_ row: TrackFacts.Row) -> String {
        row.isServerDefault ? "\(row.summary) (default)" : row.summary
    }

    private func isChosenAudio(_ row: TrackFacts.Row) -> Bool {
        (selection.audioIndex ?? file.playbackDefaults?.audio?.selectedIndex) == row.index
    }

    private func isChosenSubtitle(_ row: TrackFacts.Row) -> Bool {
        (selection.subtitleIndex ?? file.playbackDefaults?.subtitle?.selectedIndex) == row.index
    }

    private var isChosenSubtitleOff: Bool {
        if let chosen = selection.subtitleIndex { return chosen == PrePlaySelection.subtitleOff }
        return file.playbackDefaults?.subtitle?.selectedIndex == nil
    }

    @ViewBuilder
    private func choiceMenu<Content: View>(
        title: String,
        symbol: String,
        value: String,
        @ViewBuilder content: () -> Content
    ) -> some View {
        Menu {
            content()
        } label: {
            #if os(tvOS)
            Text("\(title) · \(value)")
                .font(.system(.body, design: .monospaced))
                .lineLimit(1)
            #else
            HStack(spacing: 6) {
                Image(systemName: symbol)
                    .font(.system(size: 12, weight: .semibold))
                Text(value)
                    .lineLimit(1)
            }
            .font(.subheadline.weight(.semibold))
            #endif
        }
        .accessibilityLabel("\(title): \(value)")
        #if os(tvOS)
        .buttonStyle(TVReadableButtonStyle(prominent: false))
        #endif
    }
}

/// The burn-in disclosure for a pre-play subtitle choice. Shown on the detail
/// screen, before playback starts, because after playback starts is too late
/// to decide whether the cost is worth paying.
struct TrackChoiceCostNotice: View {
    let file: MediaFile
    let selection: PrePlaySelection

    var body: some View {
        if let warning = TrackFacts.burnInWarning(file, chosen: selection.subtitleIndex) {
            Label(warning, systemImage: "exclamationmark.triangle.fill")
                .font(.system(.caption, design: .rounded))
                .foregroundStyle(Palette.muted)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: EpisodeMediaInfoMetrics.maximumWidth, alignment: .leading)
        }
    }
}
