//! GDM ("G'Day Mate") local discovery responder logic.
//!
//! Plex clients multicast `M-SEARCH * HTTP/1.0` to 239.0.0.250:32414; the
//! server unicasts back an HTTP-like descriptor. This module is the pure
//! request-classify + response-build half; the UDP socket lives in plurxd and
//! must only bind LAN interfaces (never answer discovery from the WAN —
//! GDM/SSDP has been abused for reflection DDoS).

pub const GDM_MULTICAST_ADDR: &str = "239.0.0.250";
pub const GDM_PORT: u16 = 32414;

/// Does this datagram look like a GDM server search?
pub fn is_search(payload: &[u8]) -> bool {
    let text = String::from_utf8_lossy(payload);
    text.starts_with("M-SEARCH")
}

/// Build the GDM response advertising this server. `port` is where the Plex
/// API is actually served (plurx uses one port for everything).
pub fn response(machine_identifier: &str, name: &str, version: &str, port: u16) -> Vec<u8> {
    // CRLF-separated HTTP/1.0-style headers. Clients dedupe on
    // Resource-Identifier, which must match /identity's machineIdentifier.
    let body = format!(
        "HTTP/1.0 200 OK\r\n\
         Content-Type: plex/media-server\r\n\
         Resource-Identifier: {machine_identifier}\r\n\
         Name: {name}\r\n\
         Port: {port}\r\n\
         Version: {version}\r\n\
         Server-Class: \r\n\r\n"
    );
    body.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_search() {
        assert!(is_search(b"M-SEARCH * HTTP/1.0\r\n\r\n"));
        assert!(!is_search(b"HELLO"));
        assert!(!is_search(b""));
    }

    #[test]
    fn response_carries_identity_and_port() {
        let r = response("abc123", "den", "0.0.2", 32600);
        let text = String::from_utf8(r).expect("utf8");
        assert!(text.starts_with("HTTP/1.0 200 OK"));
        assert!(text.contains("Content-Type: plex/media-server"));
        assert!(text.contains("Resource-Identifier: abc123"));
        assert!(text.contains("Name: den"));
        assert!(text.contains("Port: 32600"));
    }
}
