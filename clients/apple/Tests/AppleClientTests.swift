import Darwin
import Foundation
import SwiftUI
import UIKit
import XCTest
@testable import plurx

final class AppleClientTests: XCTestCase {
    override func tearDown() {
        Session.shared.origin = ""
        Session.shared.token = nil
        super.tearDown()
    }

    func testOriginNormalizationAcceptsHostnamesAndRemovesTrailingSlashes() {
        XCTAssertEqual(AppModel.normalizeOrigin("  media-box:32400///  "), "http://media-box:32400")
        XCTAssertEqual(AppModel.normalizeOrigin("media-box"), "http://media-box:32400")
        XCTAssertEqual(AppModel.normalizeOrigin("192.168.1.20"), "http://192.168.1.20:32400")
        XCTAssertEqual(AppModel.normalizeOrigin("http://192.168.1.20"), "http://192.168.1.20:32400")
        XCTAssertEqual(AppModel.normalizeOrigin("https://media.example.test/"), "https://media.example.test")
        XCTAssertEqual(AppModel.normalizeOrigin("   "), "")
    }

    func testConnectionCodesAcceptServerAddressesAndRejectUnrelatedPayloads() {
        XCTAssertEqual(
            ConnectionCode.origin(from: "http://192.168.4.10:32400/"),
            "http://192.168.4.10:32400"
        )
        XCTAssertEqual(
            ConnectionCode.origin(
                from: "plurx://connect?origin=http%3A%2F%2Fmedia-box%3A32400"
            ),
            "http://media-box:32400"
        )
        XCTAssertNil(ConnectionCode.origin(from: "https://example.com/not-a-server-page"))
        XCTAssertNil(ConnectionCode.origin(from: "wifi password"))
    }

    func testSavedServerIdentityMatchesExactlyAndMigratesLegacyBonjourHosts() {
        let instanceId = "4f2cfb82-9162-4be0-a8bb-0123456789ab"
        XCTAssertTrue(AppModel.matchesSavedServer(
            candidateInstanceId: instanceId,
            expectedInstanceId: instanceId,
            savedOrigin: "http://old-address:32400"
        ))
        XCTAssertTrue(AppModel.matchesSavedServer(
            candidateInstanceId: instanceId,
            expectedInstanceId: nil,
            savedOrigin: "http://plurx-4f2cfb829162.local:32400"
        ))
        XCTAssertFalse(AppModel.matchesSavedServer(
            candidateInstanceId: "different-server",
            expectedInstanceId: instanceId,
            savedOrigin: "http://plurx-4f2cfb829162.local:32400"
        ))
    }

    func testSessionTokenMovesOutOfDefaultsAndSurvivesPreferenceReplacement() throws {
        final class MemoryTokenStore: TokenStoring {
            var value: String?
            func read() -> String? { value }
            func write(_ token: String) -> Bool { value = token; return true }
            func clear() { value = nil }
        }

        let suite = "tv.plurx.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let vault = MemoryTokenStore()

        defaults.set("legacy-token", forKey: "plurx.token")
        let migrated = SettingsStore(defaults: defaults, tokenVault: vault)
        XCTAssertEqual(migrated.token, "legacy-token")
        XCTAssertEqual(vault.value, "legacy-token")
        XCTAssertNil(defaults.string(forKey: "plurx.token"))

        defaults.removePersistentDomain(forName: suite)
        let restored = SettingsStore(defaults: defaults, tokenVault: vault)
        XCTAssertEqual(restored.token, "legacy-token")

        restored.clearToken()
        XCTAssertNil(restored.token)
    }

    func testBonjourOriginsHandleDnsNamesAndIpv6() {
        XCTAssertEqual(BonjourAddress.origin(host: "media-box.local.", port: 32400),
                       "http://media-box.local:32400")
        XCTAssertEqual(BonjourAddress.origin(host: "fe80::1", port: 32400),
                       "http://[fe80::1]:32400")
        XCTAssertEqual(BonjourAddress.origin(host: "fe80::1%en0", port: 32400),
                       "http://[fe80::1%25en0]:32400")
    }

    func testBonjourResolutionPrefersAFreshNumericAddress() {
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        XCTAssertEqual(
            "192.168.4.42".withCString { inet_pton(AF_INET, $0, &address.sin_addr) },
            1
        )
        let data = Data(bytes: &address, count: MemoryLayout<sockaddr_in>.size)
        XCTAssertEqual(BonjourAddress.numericHost(from: [data]), "192.168.4.42")
    }

    func testRelativeMediaURLCarriesTokenAndPreservesExistingQuery() throws {
        Session.shared.origin = "http://media-box:32400"
        Session.shared.token = "secret token"

        let url = try XCTUnwrap(Session.shared.mediaURL("/api/v1/files/42/direct?download=1"))
        let components = try XCTUnwrap(URLComponents(url: url, resolvingAgainstBaseURL: false))
        let query = Dictionary(uniqueKeysWithValues: (components.queryItems ?? []).map { ($0.name, $0.value) })

        XCTAssertEqual(components.scheme, "http")
        XCTAssertEqual(components.host, "media-box")
        XCTAssertEqual(components.port, 32400)
        XCTAssertEqual(query["download"], "1")
        XCTAssertEqual(query["token"], "secret token")
    }

    func testAuthorizationHeaderUsesTheCurrentSessionToken() throws {
        Session.shared.token = "bearer-token"
        var request = URLRequest(url: try XCTUnwrap(URL(string: "https://media.example.test/api/v1/me")))

        Session.shared.authorize(&request)

        XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer bearer-token")
    }

    func testAutoHlsRequestLeavesHeightUnsetAndCreatesAnIdempotencyKey() throws {
        let request = CreateSessionRequest(playbackId: "player-1", start: 12.5)
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        let json = try XCTUnwrap(
            JSONSerialization.jsonObject(with: encoder.encode(request)) as? [String: Any]
        )

        XCTAssertEqual(json["playback_id"] as? String, "player-1")
        XCTAssertEqual(json["start"] as? Double, 12.5)
        XCTAssertNotNil(json["request_id"] as? String)
        XCTAssertNil(json["height"])
    }

    func testAppleCapsDescribeDolbyVisionProfilesWithoutDeprecatedHDRAPI() {
        let caps = Dictionary(uniqueKeysWithValues: Caps.query().compactMap { item in
            item.value.map { (item.name, $0) }
        })

        XCTAssertNotNil(caps["hdr"])
        XCTAssertNotNil(caps["dv"])
        XCTAssertNotNil(caps["dvprofile"])
        if caps["dv"] == "1" {
            XCTAssertEqual(caps["dvprofile"], "5,8")
        }
    }

    func testPictureInPictureCommandStartsStopsAndWaitsForAvailability() {
        XCTAssertEqual(
            PictureInPictureController.command(isActive: false, isPossible: true),
            .start
        )
        XCTAssertEqual(
            PictureInPictureController.command(isActive: true, isPossible: false),
            .stop
        )
        XCTAssertEqual(
            PictureInPictureController.command(isActive: false, isPossible: false),
            .unavailable
        )
    }

    func testAppDeclaresBackgroundAudioForPictureInPicture() {
        let modes = Bundle.main.object(forInfoDictionaryKey: "UIBackgroundModes") as? [String] ?? []
        XCTAssertTrue(modes.contains("audio"))
    }

    func testCopySessionCanPreserveNegotiatedDolbyVision() throws {
        let request = CreateSessionRequest(
            playbackId: "player-dv",
            copy: true,
            aac: false,
            preserveDolbyVision: true
        )
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        let json = try XCTUnwrap(
            JSONSerialization.jsonObject(with: encoder.encode(request)) as? [String: Any]
        )

        XCTAssertEqual(json["preserve_dolby_vision"] as? Bool, true)
    }

    func testPlayContextKeepsTheServerDurationForProgressReporting() {
        let context = PlayContext(itemId: 7, fileId: 11, startMs: 3_000,
                                  durationMs: 7_200_000, title: "Feature",
                                  overview: "A precise description.")

        XCTAssertEqual(context.durationMs, 7_200_000)
        XCTAssertEqual(context.overview, "A precise description.")
    }

    func testPlayerFactsSurfaceUsefulSourceInformation() {
        let source = SourceSummary(
            container: nil,
            videoCodec: "hevc",
            videoProfile: nil,
            width: nil,
            height: 2160,
            bitDepth: nil,
            hdr: nil,
            hdrFormat: "Dolby Vision · Profile 8",
            bitrate: nil,
            durationMs: nil
        )
        let audio = AudioTrack(
            index: 0,
            codec: "eac3",
            channels: 6,
            language: "eng",
            title: nil,
            default: true
        )

        XCTAssertEqual(
            PlayerView.playbackFacts(source: source, audio: audio),
            ["4K", "Dolby Vision", "Dolby Digital Plus 5.1 channels"]
        )
        XCTAssertEqual(
            PlayerView.playbackBadges(source: source, audio: audio).map(\.mark),
            [nil, "DV", "DD+ 5.1"]
        )
        XCTAssertEqual(
            PlayerView.playbackBadges(source: source, audio: audio).map(\.symbol),
            ["4k.tv.fill", "sparkles", "speaker.wave.3.fill"]
        )
    }

    func testPlayerOverlayAutoHidesWheneverItIsIdle() {
        XCTAssertFalse(PlayerView.shouldAutoHideControls(
            visible: true,
            scrubbing: true,
            changingStream: false
        ))
        XCTAssertTrue(PlayerView.shouldAutoHideControls(
            visible: true,
            scrubbing: false,
            changingStream: false
        ))
        XCTAssertFalse(PlayerView.shouldAutoHideControls(
            visible: false,
            scrubbing: false,
            changingStream: false
        ))
    }

    func testNowPlayingSummaryUsesLoadedOverviewAndHasAFallback() {
        XCTAssertEqual(
            PlayerView.nowPlayingSummary("  A family crosses the stars.\n"),
            "A family crosses the stars."
        )
        XCTAssertEqual(PlayerView.nowPlayingSummary("  "), "No description available.")
        XCTAssertEqual(PlayerView.nowPlayingSummary(nil), "No description available.")
    }

    func testPlayerOffersDownCueUntilNowPlayingInfoIsVisible() {
        XCTAssertEqual(PlayerView.nowPlayingInfoCueLabel(showingInfo: false), "INFO")
        XCTAssertNil(PlayerView.nowPlayingInfoCueLabel(showingInfo: true))
    }

    func testDetailViewportAndBodyNeverOutgrowTheirAvailableWidth() {
        for availableWidth: CGFloat in [320, 390, 430, 744, 1_366] {
            let controller = UIHostingController(rootView: DetailViewportFrame {
                DetailBodyFrame {
                    Text(String(repeating: "A wide detail overview. ", count: 20))
                        .fixedSize(horizontal: false, vertical: true)
                }
            })
            let measured = controller.sizeThatFits(
                in: CGSize(width: availableWidth, height: 10_000)
            )

            XCTAssertLessThanOrEqual(measured.width, availableWidth + 0.5)
        }
    }

    func testEpisodeBreadcrumbLinksToTheShowAndSeasonInOrder() {
        let show = Item(id: 10, kind: "show", title: "Shameless")
        let season = Item(id: 20, kind: "season", title: "Season 1")

        XCTAssertEqual(
            [show, season].map(DetailBreadcrumb.destination(for:)),
            [.item(10), .item(20)]
        )
    }

    #if os(tvOS)
    func testTVHomeStartsWithMediaRailsInsteadOfAFeaturedBillboard() {
        XCTAssertFalse(HomeLayoutPolicy.usesFeaturedHero)
        XCTAssertEqual(HomeLayoutPolicy.continueWatchingCopyStyle, .accentPanel)
        XCTAssertEqual(
            HomeLayoutPolicy.topLevelTabs,
            ["Home", "Libraries", "Search", "Settings"]
        )
        XCTAssertFalse(HomeLayoutPolicy.showsLibraryShelvesOnHome)
        XCTAssertLessThanOrEqual(LandscapeAccentPanelMetrics.fillOpacity, 0.06)
        XCTAssertLessThanOrEqual(LandscapeAccentPanelMetrics.strokeOpacity, 0.18)
        XCTAssertLessThanOrEqual(LandscapeAccentPanelMetrics.strokeWidth, 0.5)
    }

    func testTVSeriesDetailKeepsCorrectArtworkRatioAndVisibleChildShelf() {
        XCTAssertEqual(
            TVSeriesDetailMetrics.posterHeight / TVSeriesDetailMetrics.posterWidth,
            1.5,
            accuracy: 0.001
        )
        XCTAssertLessThanOrEqual(
            TVSeriesDetailMetrics.headerHeight + 320,
            900,
            "the first row must begin inside the usable area below the tvOS tab bar"
        )
        XCTAssertEqual(DetailView.tvSeriesChildStyle(for: "show"), .poster)
        XCTAssertEqual(DetailView.tvSeriesChildStyle(for: "season"), .episode)
    }

    func testTVPlayableDetailUsesOneCinematicViewportAndUsefulMetadata() throws {
        XCTAssertLessThanOrEqual(
            TVPlayableDetailMetrics.heroHeight,
            720,
            "title, synopsis, and actions must remain visible below the tvOS tab bar"
        )
        XCTAssertGreaterThanOrEqual(TVPlayableDetailMetrics.copyWidth, 800)
        XCTAssertLessThanOrEqual(
            TVPlayableDetailMetrics.bottomInset,
            32,
            "the information group belongs against the lower television edge"
        )

        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let item = try decoder.decode(
            Item.self,
            from: Data(#"{"id":7,"kind":"episode","title":"Fray","year":2026,"season_number":4,"episode_number":2,"runtime_ms":3245000}"#.utf8)
        )
        let file = try decoder.decode(
            MediaFile.self,
            from: Data(#"{"id":11,"duration_ms":3245000,"container":"mkv","video_codec":"hevc","height":2160}"#.utf8)
        )

        XCTAssertEqual(
            DetailView.tvPlayableMetadata(item, file: file, durationMs: file.durationMs),
            "Season 4, Episode 2   ·   54 min   ·   4K   ·   HEVC"
        )
        XCTAssertEqual(
            DetailView.tvPlayableMetadataParts(item, file: file, durationMs: file.durationMs),
            ["Season 4, Episode 2", "54 min", "4K", "HEVC"]
        )
        let badges = DetailView.itemMetadataBadges(
            item,
            file: file,
            durationMs: file.durationMs,
            includeSeries: false
        )
        XCTAssertEqual(
            badges.map(\.symbol),
            ["rectangle.stack.fill", "clock.fill", "4k.tv.fill", "film.fill"]
        )
        XCTAssertEqual(badges.map(\.accessibilityLabel), [
            "Season 4, Episode 2", "54 min", "4K", "HEVC",
        ])
    }

    func testTVActionButtonsRemainReadableWithAndWithoutFocus() {
        for prominent in [false, true] {
            for focused in [false, true] {
                let foreground = TVReadableButtonStyle.foregroundColor(
                    prominent: prominent,
                    focused: focused
                )
                let background = TVReadableButtonStyle.backgroundColor(
                    prominent: prominent,
                    focused: focused
                )

                XCTAssertGreaterThanOrEqual(
                    contrastRatio(foreground, background),
                    4.5,
                    "prominent=\(prominent), focused=\(focused)"
                )
            }
        }
    }

    func testTVShelfActionsRemainReadableWithAndWithoutFocus() {
        for focused in [false, true] {
            XCTAssertGreaterThanOrEqual(
                contrastRatio(
                    TVShelfActionButtonStyle.foregroundColor(focused: focused),
                    TVShelfActionButtonStyle.backgroundColor(focused: focused)
                ),
                4.5,
                "focused=\(focused)"
            )
        }
    }

    func testTVMediaCardFocusSurroundIsThinAndFadesAtBothEdges() {
        XCTAssertLessThanOrEqual(
            TVMediaCardButtonStyle.outerStrokeWidth,
            6,
            "the complete focus surround should remain a thin ring"
        )
        XCTAssertGreaterThan(
            TVMediaCardButtonStyle.outerStrokeWidth,
            TVMediaCardButtonStyle.fadeStrokeWidth
        )
        XCTAssertGreaterThan(
            TVMediaCardButtonStyle.fadeStrokeWidth,
            TVMediaCardButtonStyle.accentStrokeWidth
        )
        XCTAssertGreaterThan(
            TVMediaCardButtonStyle.contentClearance,
            TVMediaCardButtonStyle.outerStrokeWidth / 2,
            "the curved ring must stay outside card text"
        )
        XCTAssertEqual(
            TVMediaCardButtonStyle.outerStrokeWidth - TVMediaCardButtonStyle.fadeStrokeWidth,
            TVMediaCardButtonStyle.fadeStrokeWidth - TVMediaCardButtonStyle.accentStrokeWidth,
            "the red core should taper evenly toward both black edges"
        )
    }

    func testTVPlayerChromeStaysCompactWithAHairlineProgressFocusRing() {
        XCTAssertLessThanOrEqual(TVPlayerControlButtonStyle.width, 54)
        XCTAssertLessThanOrEqual(TVPlayerControlButtonStyle.height, 46)
        XCTAssertLessThanOrEqual(TVPlayerChromeMetrics.headerHorizontalInset, 8)
        XCTAssertLessThanOrEqual(TVPlayerChromeMetrics.headerVerticalInset, 5)
        XCTAssertLessThanOrEqual(TVPlayerChromeMetrics.timeHorizontalInset, 6)
        XCTAssertLessThanOrEqual(TVPlayerChromeMetrics.timeVerticalInset, 3)
        XCTAssertGreaterThanOrEqual(TVPlayerChromeMetrics.infoBodyFontSize, 18)
        XCTAssertLessThanOrEqual(TVPlayerChromeMetrics.infoBodyFontSize, 20)
        XCTAssertGreaterThanOrEqual(TVPlayerChromeMetrics.infoLineLimit, 6)
        XCTAssertLessThanOrEqual(TVPlayerProgressFocusRing.outerStrokeWidth, 1.5)
        XCTAssertGreaterThan(
            TVPlayerProgressFocusRing.outerStrokeWidth,
            TVPlayerProgressFocusRing.fadeStrokeWidth
        )
        XCTAssertGreaterThan(
            TVPlayerProgressFocusRing.fadeStrokeWidth,
            TVPlayerProgressFocusRing.accentStrokeWidth
        )
    }

    func testShelfMetadataUsesMediaFactsInsteadOfLibraryCategory() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let episode = try decoder.decode(
            Item.self,
            from: Data(#"{"id":1,"kind":"episode","title":"Fray","season_number":4,"episode_number":2,"year":2026,"watch":{"position_ms":600000,"duration_ms":3240000}}"#.utf8)
        )
        let movie = try decoder.decode(
            Item.self,
            from: Data(#"{"id":2,"kind":"movie","title":"TRON: Ares","year":2025,"resolution":2160,"watch":{"position_ms":300000,"duration_ms":7200000}}"#.utf8)
        )

        XCTAssertEqual(cardShelfMetadata(episode), "S4 E2 · 44m left")
        XCTAssertEqual(cardShelfMetadata(movie), "2025 · 115m left")
        XCTAssertEqual(resolutionLabel(movie.resolution), "4K")
        XCTAssertFalse(cardShelfMetadata(movie).contains("4K"))
        XCTAssertFalse(cardShelfMetadata(episode).contains("TV"))
        XCTAssertFalse(cardShelfMetadata(movie).contains("Movies"))
    }

    func testMixedLandscapeShelfReservesEpisodeSubtitleLine() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let movie = try decoder.decode(
            Item.self,
            from: Data(#"{"id":1,"kind":"movie","title":"TRON: Ares","year":2025}"#.utf8)
        )
        let episode = try decoder.decode(
            Item.self,
            from: Data(#"{"id":2,"kind":"episode","title":"Fray","show_title":"FROM","season_number":4,"episode_number":2}"#.utf8)
        )

        XCTAssertFalse(landscapeShelfNeedsEpisodeSubtitleLine([movie]))
        XCTAssertTrue(landscapeShelfNeedsEpisodeSubtitleLine([movie, episode]))
    }

    func testContinueWatchingUsesTwoRowsAndRightAlignedTimeCopy() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let episode = try decoder.decode(
            Item.self,
            from: Data(#"{"id":1,"kind":"episode","title":"Fray","show_title":"FROM","season_number":4,"episode_number":2,"watch":{"position_ms":600000,"duration_ms":3240000}}"#.utf8)
        )
        let movie = try decoder.decode(
            Item.self,
            from: Data(#"{"id":2,"kind":"movie","title":"TRON: Ares","year":2025,"watch":{"position_ms":300000,"duration_ms":7200000}}"#.utf8)
        )

        XCTAssertEqual(continueWatchingDetail(episode), "S4 E2 · Fray")
        XCTAssertEqual(continueWatchingTimeRemaining(episode), "44m left")
        XCTAssertEqual(continueWatchingDetail(movie), "2025")
        XCTAssertEqual(continueWatchingTimeRemaining(movie), "115m left")
    }

    private func contrastRatio(_ foreground: Color, _ background: Color) -> CGFloat {
        let lighter = max(relativeLuminance(foreground), relativeLuminance(background))
        let darker = min(relativeLuminance(foreground), relativeLuminance(background))
        return (lighter + 0.05) / (darker + 0.05)
    }

    private func relativeLuminance(_ color: Color) -> CGFloat {
        var red: CGFloat = 0
        var green: CGFloat = 0
        var blue: CGFloat = 0
        var alpha: CGFloat = 0
        XCTAssertTrue(UIColor(color).getRed(&red, green: &green, blue: &blue, alpha: &alpha))

        func linear(_ component: CGFloat) -> CGFloat {
            component <= 0.04045
                ? component / 12.92
                : pow((component + 0.055) / 1.055, 2.4)
        }

        return 0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
    }
    #endif

    func testNativeItemDecodesLibraryProvenanceAndSortFields() throws {
        let json = #"{"id":7,"library_id":12,"kind":"movie","title":"Feature","added_at":99,"updated_at":101}"#
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase

        let item = try decoder.decode(Item.self, from: Data(json.utf8))

        XCTAssertEqual(item.libraryId, 12)
        XCTAssertEqual(item.addedAt, 99)
        XCTAssertEqual(item.updatedAt, 101)
    }

    func testWatchFiltersMatchWebClientSemantics() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        func item(_ watch: String) throws -> Item {
            let json = "{\"id\":1,\"kind\":\"movie\",\"title\":\"Feature\",\"watch\":\(watch)}"
            return try decoder.decode(Item.self, from: Data(json.utf8))
        }
        let unwatched = try item("{\"position_ms\":0,\"watched\":false}")
        let progressing = try item("{\"position_ms\":4000,\"watched\":false}")
        let watched = try item("{\"position_ms\":0,\"watched\":true}")

        XCTAssertTrue(AppModel.matches(unwatched, filter: .unwatched))
        XCTAssertTrue(AppModel.matches(progressing, filter: .inProgress))
        XCTAssertTrue(AppModel.matches(watched, filter: .watched))
        XCTAssertFalse(AppModel.matches(progressing, filter: .unwatched))
    }

    func testPluralServerLibraryKindsMapToNativeMovieAndTVTabs() {
        XCTAssertEqual(AppModel.canonicalLibraryKind("movies"), "movie")
        XCTAssertEqual(AppModel.canonicalLibraryKind("shows"), "show")
        XCTAssertEqual(AppModel.canonicalLibraryKind("home"), "home")
        XCTAssertEqual(AppModel.canonicalLibraryKind("MOVIES"), "movie")
    }

    func testPosterSizesAreOrderedAndMatchTheWebChoices() {
        XCTAssertEqual(PosterSize.allCases.map(\.label), ["Small", "Medium", "Large", "Extra large"])
        XCTAssertLessThan(PosterSize.small.posterWidth, PosterSize.medium.posterWidth)
        XCTAssertLessThan(PosterSize.medium.posterWidth, PosterSize.large.posterWidth)
        XCTAssertLessThan(PosterSize.large.posterWidth, PosterSize.extraLarge.posterWidth)
    }

    func testCancelledURLRequestRemainsCancellationInsteadOfConnectionFailure() {
        let mapped = PlurxAPI.transportError(from: URLError(.cancelled))

        XCTAssertTrue(mapped is CancellationError)
        XCTAssertNil(AppModel.homeErrorMessage(for: mapped, hasCachedContent: false))
    }

    func testTransientRefreshFailureKeepsCachedHomeContentVisible() {
        let failure = APIError.transport("The request timed out.")

        XCTAssertNil(AppModel.homeErrorMessage(for: failure, hasCachedContent: true))
        XCTAssertEqual(
            AppModel.homeErrorMessage(for: failure, hasCachedContent: false),
            "The request timed out."
        )
    }
}
