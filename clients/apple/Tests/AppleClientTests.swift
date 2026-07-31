import Foundation
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
        XCTAssertEqual(AppModel.normalizeOrigin("https://media.example.test/"), "https://media.example.test")
        XCTAssertEqual(AppModel.normalizeOrigin("   "), "")
    }

    func testBonjourOriginsHandleDnsNamesAndIpv6() {
        XCTAssertEqual(BonjourAddress.origin(host: "media-box.local.", port: 32400),
                       "http://media-box.local:32400")
        XCTAssertEqual(BonjourAddress.origin(host: "fe80::1", port: 32400),
                       "http://[fe80::1]:32400")
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
                                  durationMs: 7_200_000, title: "Feature")

        XCTAssertEqual(context.durationMs, 7_200_000)
    }

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
}
