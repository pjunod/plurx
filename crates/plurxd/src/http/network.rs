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
pub(crate) fn identity(
    headers: &HeaderMap,
    remote: Option<SocketAddr>,
    client_hint: Option<&str>,
) -> Option<NetworkIdentity> {
    let address = forwarded_ipv4(headers).or_else(|| match remote?.ip() {
        IpAddr::V4(address) => Some(address),
        IpAddr::V6(_) => None,
    })?;
    let [a, b, c, _] = address.octets();
    Some(NetworkIdentity {
        client_class: client_class(client_hint, headers),
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

fn client_class(client_hint: Option<&str>, headers: &HeaderMap) -> String {
    let hint = client_hint.unwrap_or_default().trim().to_ascii_lowercase();
    if hint.contains("apple") || hint.contains("avplayer") {
        return "apple".to_owned();
    }
    if hint.contains("android") || hint.contains("media3") {
        return "android".to_owned();
    }
    for (needle, class) in [
        ("edge", "edge"),
        ("firefox", "firefox"),
        ("chrome", "chrome"),
        ("safari", "safari"),
    ] {
        if hint.contains(needle) {
            return class.to_owned();
        }
    }
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ua.contains("avplayer")
        || ua.contains("apple")
        || ua.contains("cfnetwork")
        || ua.contains("darwin/")
    {
        return "apple".to_owned();
    }
    if ua.contains("media3") || ua.contains("android") || ua.contains("okhttp/") {
        return "android".to_owned();
    }
    // Chromium-based Edge says both Edg and Chrome, so order is meaningful.
    for (needle, class) in [
        ("edg/", "edge"),
        ("firefox/", "firefox"),
        ("chrome/", "chrome"),
        ("safari/", "safari"),
    ] {
        if ua.contains(needle) {
            return class.to_owned();
        }
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
    use axum::http::HeaderValue;

    #[test]
    fn identity_keeps_only_the_ua_class_and_ipv4_slash_24() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("192.0.2.143, 10.0.0.4"),
        );
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static("Mozilla/5.0 Chrome/140.0 Safari/537.36"),
        );
        let identity = identity(&headers, None, None).expect("identity");
        assert_eq!(identity.client_class, "chrome");
        assert_eq!(identity.network_fingerprint, "192.0.2.0/24");
        assert!(!identity.network_fingerprint.contains("143"));
    }

    #[test]
    fn native_hint_wins_and_direct_lan_falls_back_to_the_peer() {
        let headers = HeaderMap::new();
        let identity = identity(
            &headers,
            Some("10.23.45.67:1234".parse().expect("peer")),
            Some("Apple AVPlayer"),
        )
        .expect("identity");
        assert_eq!(identity.client_class, "apple");
        assert_eq!(identity.network_fingerprint, "10.23.45.0/24");

        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static("plurx/2 CFNetwork/1498.700.2 Darwin/23.6.0"),
        );
        assert_eq!(
            client_class(None, &headers),
            "apple",
            "URLSession requests must join Apple telemetry's class"
        );
        headers.insert(header::USER_AGENT, HeaderValue::from_static("okhttp/5.1.0"));
        assert_eq!(
            client_class(None, &headers),
            "android",
            "Retrofit requests must join Media3 telemetry's class"
        );
    }

    #[test]
    fn ipv6_is_not_silently_widened_beyond_the_ratified_contract() {
        let headers = HeaderMap::new();
        assert!(identity(
            &headers,
            Some("[2001:db8::1]:1234".parse().expect("peer")),
            None,
        )
        .is_none());
    }
}
