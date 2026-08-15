//! Privacy-bounded request identity for node-local network priors.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap};

use plurx_core::domain::NetworkPrior;
use plurx_core::store::{keys, Store};

use super::error::ApiError;
use crate::state::AppState;
use crate::telemetry::NetworkIdentity;

/// Optional socket peer. Production inserts [`ConnectInfo`]; router unit tests
/// intentionally do not, and must retain the default-off behavior unchanged.
pub(crate) struct RemoteAddress(pub(crate) Option<SocketAddr>);

impl FromRequestParts<AppState> for RemoteAddress {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(address)| *address),
        ))
    }
}

/// Reduce a request to the deliberately coarse key ratified for N4.2.
///
/// Proxy headers are considered in their conventional order, then the socket
/// peer is used for direct LAN operation. Only IPv4 is admitted because the
/// contract specifies a /24; silently inventing a finer IPv6 identity would
/// widen the tracking surface.
pub(crate) fn identity(headers: &HeaderMap, remote: Option<SocketAddr>) -> Option<NetworkIdentity> {
    let address = forwarded_ipv4(headers).or_else(|| match remote?.ip() {
        IpAddr::V4(address) => Some(address),
        IpAddr::V6(_) => None,
    })?;
    let [a, b, c, _] = address.octets();
    Some(NetworkIdentity {
        client_class: client_class(headers),
        network_fingerprint: format!("{a}.{b}.{c}.0/24"),
    })
}

fn forwarded_ipv4(headers: &HeaderMap) -> Option<Ipv4Addr> {
    if let Some(value) = headers
        .get("forwarded")
        .and_then(|value| value.to_str().ok())
    {
        for entry in value.split(',') {
            for parameter in entry.split(';') {
                let Some(raw) = parameter.trim().strip_prefix("for=") else {
                    continue;
                };
                if let Some(address) = parse_ipv4(raw) {
                    return Some(address);
                }
            }
        }
    }
    for name in ["x-forwarded-for", "x-real-ip"] {
        let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) else {
            continue;
        };
        for raw in value.split(',') {
            if let Some(address) = parse_ipv4(raw) {
                return Some(address);
            }
        }
    }
    None
}

fn parse_ipv4(raw: &str) -> Option<Ipv4Addr> {
    let raw = raw.trim().trim_matches('"');
    raw.parse::<Ipv4Addr>().ok().or_else(|| {
        raw.parse::<SocketAddr>()
            .ok()
            .and_then(|value| match value.ip() {
                IpAddr::V4(address) => Some(address),
                IpAddr::V6(_) => None,
            })
    })
}

/// Derive the coarse client class from the request's own `User-Agent`, and
/// from nothing else.
///
/// One derivation for every path on purpose. A per-call-site hint used to
/// override this, and the write and read paths did not pass the same one:
/// `/client-log` forwarded the web player's `browserLabel()` and wrote under
/// `chrome`, while session-create passed no hint at all and read under the
/// header's class. `client_class` is part of the prior's primary key, so a key
/// written one way and read another is a prior that is maintained forever and
/// never consulted (review finding 1). The header is the one input every path
/// already has, so deriving from it alone makes the two sides agree by
/// construction rather than by convention.
fn client_class(headers: &HeaderMap) -> String {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    // Browsers are tested first because every native needle is a substring of
    // some shipping browser's User-Agent: Chromium and WebKit both ship
    // `AppleWebKit`, and Chrome on a phone ships `Android`. Testing the native
    // family first classified every Chromium and WebKit browser as `apple`.
    // Order matters inside this list too — Edge says both `Edg/` and
    // `Chrome/`, and Chrome says both `Chrome/` and `Safari/`.
    for (needle, class) in [
        ("edg/", "edge"),
        ("edga/", "edge"),
        ("edgios/", "edge"),
        ("firefox/", "firefox"),
        ("fxios/", "firefox"),
        ("crios/", "chrome"),
        ("chrome/", "chrome"),
        ("chromium/", "chrome"),
        ("safari/", "safari"),
    ] {
        if ua.contains(needle) {
            return class.to_owned();
        }
    }
    // Native players and the HTTP stacks the native clients default to.
    if ua.contains("avplayer")
        || ua.contains("applecoremedia")
        || ua.contains("cfnetwork")
        || ua.contains("darwin/")
    {
        return "apple".to_owned();
    }
    if ua.contains("media3")
        || ua.contains("exoplayer")
        || ua.contains("okhttp/")
        || ua.contains("android")
    {
        return "android".to_owned();
    }
    "other".to_owned()
}

pub(crate) async fn stored_prior(
    store: &dyn Store,
    user_id: i64,
    identity: Option<&NetworkIdentity>,
) -> Result<Option<NetworkPrior>, ApiError> {
    let enabled = store
        .get_setting(keys::PLAYBACK_NETWORK_PRIORS)
        .await?
        .is_some_and(|value| value.trim() == "1");
    let Some(identity) = identity.filter(|_| enabled) else {
        return Ok(None);
    };
    Ok(store
        .network_prior(
            user_id,
            &identity.client_class,
            &identity.network_fingerprint,
        )
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::test_agents::*;
    use axum::http::HeaderValue;

    fn class_of(user_agent: &'static str) -> String {
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, HeaderValue::from_static(user_agent));
        client_class(&headers)
    }

    #[test]
    fn identity_keeps_only_the_ua_class_and_ipv4_slash_24() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("192.0.2.143, 10.0.0.4"),
        );
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static(CHROME_WINDOWS_UA),
        );
        let identity = identity(&headers, None).expect("identity");
        assert_eq!(identity.client_class, "chrome");
        assert_eq!(identity.network_fingerprint, "192.0.2.0/24");
        assert!(!identity.network_fingerprint.contains("143"));
    }

    /// Unmodified, shipping User-Agent strings, because the doctored one is
    /// what hid the defect: the old fallback tested a bare `apple` substring
    /// first, and every Chromium and WebKit browser sends `AppleWebKit`, so
    /// every browser but Firefox was classified `apple`.
    #[test]
    fn real_browser_user_agents_are_not_swallowed_by_the_apple_class() {
        for (ua, expected) in [
            (CHROME_WINDOWS_UA, "chrome"),
            (CHROME_MACOS_UA, "chrome"),
            (CHROME_ANDROID_UA, "chrome"),
            (SAFARI_MACOS_UA, "safari"),
            (SAFARI_IOS_UA, "safari"),
            (EDGE_WINDOWS_UA, "edge"),
            (FIREFOX_WINDOWS_UA, "firefox"),
            (FIREFOX_IOS_UA, "firefox"),
        ] {
            assert_eq!(class_of(ua), expected, "{ua}");
        }
    }

    #[test]
    fn native_http_stacks_keep_their_own_classes_and_lan_falls_back_to_the_peer() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static(APPLE_NATIVE_UA),
        );
        let identity =
            identity(&headers, Some("10.23.45.67:1234".parse().expect("peer"))).expect("identity");
        assert_eq!(
            identity.client_class, "apple",
            "URLSession requests must join Apple telemetry's class"
        );
        assert_eq!(identity.network_fingerprint, "10.23.45.0/24");

        assert_eq!(
            class_of("AppleCoreMedia/1.0.0.21G93 (Apple TV; U; CPU OS 17_6 like Mac OS X)"),
            "apple"
        );
        assert_eq!(
            class_of(ANDROID_NATIVE_UA),
            "android",
            "Retrofit requests must join Media3 telemetry's class"
        );
        assert_eq!(class_of("curl/8.7.1"), "other");
        assert_eq!(class_of(""), "other");
    }

    #[test]
    fn ipv6_is_not_silently_widened_beyond_the_ratified_contract() {
        let headers = HeaderMap::new();
        assert!(identity(&headers, Some("[2001:db8::1]:1234".parse().expect("peer"))).is_none());
    }
}
