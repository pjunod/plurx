//! Privacy-bounded request identity for node-local network priors.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant};

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

const NETWORK_PRIOR_TOGGLE_TTL: Duration = Duration::from_secs(2);

/// Two-second snapshot of the default-off replicated toggle.
///
/// Decision and session creation share this process-wide object, so requests
/// inside one fresh window reuse the same answer. A refresh mutex collapses
/// concurrent cache misses, while a generation prevents an in-flight old read
/// from overwriting a newer admin publish. Peer changes become visible after
/// the same bounded TTL used by the other play-path settings in PERF2-PLAN
/// §11.1.
#[derive(Default)]
pub(crate) struct NetworkPriorToggle {
    cached: RwLock<Option<(Instant, bool)>>,
    refresh: tokio::sync::Mutex<()>,
    generation: AtomicU64,
}

impl NetworkPriorToggle {
    pub(crate) async fn enabled(&self, store: &dyn Store) -> Result<bool, ApiError> {
        if let Some((at, enabled)) = *self.cached.read().expect("network-prior toggle lock") {
            if at.elapsed() < NETWORK_PRIOR_TOGGLE_TTL {
                return Ok(enabled);
            }
        }
        let _refresh = self.refresh.lock().await;
        if let Some((at, enabled)) = *self.cached.read().expect("network-prior toggle lock") {
            if at.elapsed() < NETWORK_PRIOR_TOGGLE_TTL {
                return Ok(enabled);
            }
        }
        let generation = self.generation.load(Ordering::Acquire);
        let enabled = store
            .get_setting(keys::PLAYBACK_NETWORK_PRIORS)
            .await?
            .is_some_and(|value| value.trim() == "1");
        Ok(self.commit_refresh(generation, enabled))
    }

    pub(crate) fn publish(&self, enabled: bool) {
        let mut cached = self.cached.write().expect("network-prior toggle lock");
        self.generation.fetch_add(1, Ordering::AcqRel);
        *cached = Some((Instant::now(), enabled));
    }

    fn commit_refresh(&self, generation: u64, enabled: bool) -> bool {
        let mut cached = self.cached.write().expect("network-prior toggle lock");
        if self.generation.load(Ordering::Acquire) == generation {
            *cached = Some((Instant::now(), enabled));
            enabled
        } else {
            cached
                .as_ref()
                .map(|(_, published)| *published)
                .expect("a generation change always publishes a cache value")
        }
    }
}

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
/// The authenticated socket peer is authoritative in production. Forwarding
/// headers are used only when no peer exists (router tests); accepting them
/// from a directly connected client would let that client choose arbitrary
/// buckets. Only IPv4 is admitted because the contract specifies a /24;
/// silently inventing a finer IPv6 identity would widen the tracking surface.
pub(crate) fn identity(
    headers: &HeaderMap,
    remote: Option<SocketAddr>,
    client_hint: Option<&str>,
) -> Option<NetworkIdentity> {
    let address = match remote.map(|remote| remote.ip()) {
        Some(IpAddr::V4(address)) => Some(address),
        Some(IpAddr::V6(_)) => None,
        None => forwarded_ipv4(headers),
    }?;
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
    // Shipping Chromium and WebKit user agents contain `AppleWebKit`, so the
    // browser tests must precede the native Apple-family fallback. Otherwise
    // telemetry written under a browser hint can never be read by decision or
    // session requests, where only the real User-Agent is available.
    // Chromium-based Edge also says both Edg and Chrome, so order is meaningful.
    for (needle, class) in [
        ("edgios/", "edge"),
        ("edga/", "edge"),
        ("edg/", "edge"),
        ("fxios/", "firefox"),
        ("firefox/", "firefox"),
        ("crios/", "chrome"),
        ("chromium/", "chrome"),
        ("chrome/", "chrome"),
        ("safari/", "safari"),
    ] {
        if ua.contains(needle) {
            return class.to_owned();
        }
    }
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
    "other".to_owned()
}

pub(crate) async fn stored_prior(
    state: &AppState,
    user_id: i64,
    identity: Option<&NetworkIdentity>,
) -> Result<Option<NetworkPrior>, ApiError> {
    let Some(identity) = identity else {
        return Ok(None);
    };
    let enabled = state
        .network_prior_toggle
        .enabled(state.store.as_ref())
        .await?;
    if !enabled {
        return Ok(None);
    }
    Ok(state
        .store
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
    fn toggle_snapshot_ttl_is_the_binding_two_seconds() {
        assert_eq!(NETWORK_PRIOR_TOGGLE_TTL, Duration::from_secs(2));
    }

    #[test]
    fn an_in_flight_refresh_cannot_overwrite_a_newer_admin_publish() {
        let toggle = NetworkPriorToggle::default();
        let stale_generation = toggle.generation.load(Ordering::Acquire);
        toggle.publish(false);
        assert!(!toggle.commit_refresh(stale_generation, true));
        let (_, cached) = toggle
            .cached
            .read()
            .expect("network-prior toggle lock")
            .expect("published cache");
        assert!(!cached);
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
            HeaderValue::from_static("Mozilla/5.0 Chrome/140.0 Safari/537.36"),
        );
        let identity = identity(&headers, None, None).expect("identity");
        assert_eq!(identity.client_class, "chrome");
        assert_eq!(identity.network_fingerprint, "192.0.2.0/24");
        assert!(!identity.network_fingerprint.contains("143"));
    }

    #[test]
    fn shipping_browser_user_agents_do_not_collapse_into_apple() {
        let cases = [
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36",
                "chrome",
            ),
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                 AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 \
                 Safari/605.1.15",
                "safari",
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 \
                 Edg/140.0.3485.54",
                "edge",
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:141.0) \
                 Gecko/20100101 Firefox/141.0",
                "firefox",
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) \
                 AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/140.0.7339.41 \
                 Mobile/15E148 Safari/604.1",
                "chrome",
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) \
                 AppleWebKit/605.1.15 (KHTML, like Gecko) EdgiOS/140.0.3485.54 \
                 Version/18.0 Mobile/15E148 Safari/604.1",
                "edge",
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) \
                 AppleWebKit/605.1.15 (KHTML, like Gecko) FxiOS/141.0 \
                 Mobile/15E148 Safari/605.1.15",
                "firefox",
            ),
        ];
        for (ua, expected) in cases {
            let mut headers = HeaderMap::new();
            headers.insert(header::USER_AGENT, HeaderValue::from_str(ua).expect("UA"));
            assert_eq!(client_class(None, &headers), expected, "{ua}");
        }
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
    fn a_direct_peer_cannot_be_replaced_by_an_untrusted_forwarding_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.77"));
        let identity = identity(
            &headers,
            Some("192.0.2.143:4321".parse().expect("peer")),
            Some("Chrome"),
        )
        .expect("identity");
        assert_eq!(identity.network_fingerprint, "192.0.2.0/24");
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
