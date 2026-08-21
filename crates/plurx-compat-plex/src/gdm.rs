//! GDM ("G'Day Mate") local discovery responder logic.
//!
//! Plex clients multicast `M-SEARCH * HTTP/1.0` to 239.0.0.250:32414; the
//! server unicasts back an HTTP-like descriptor. This module is the pure
//! request-classify + response-build half; the UDP socket lives in plurxd and
//! must only bind LAN interfaces (never answer discovery from the WAN —
//! GDM/SSDP has been abused for reflection DDoS).

pub const GDM_MULTICAST_ADDR: &str = "239.0.0.250";
pub const GDM_PORT: u16 = 32414;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advertisement<'a> {
    pub instance_id: &'a str,
    pub name: &'a str,
    pub node_id: Option<&'a str>,
}

/// Does this datagram look like a GDM server search?
pub fn is_search(payload: &[u8]) -> bool {
    let text = String::from_utf8_lossy(payload);
    text.starts_with("M-SEARCH")
}

/// Build the GDM response advertising this server. `port` is where the Plex
/// API is actually served (plurx uses one port for everything).
pub fn response(machine_identifier: &str, name: &str, version: &str, port: u16) -> Vec<u8> {
    response_for(
        &Advertisement {
            instance_id: machine_identifier,
            name,
            node_id: None,
        },
        version,
        port,
    )
}

/// Build a response for one node under a logical server identity.
pub fn response_for(advertisement: &Advertisement<'_>, version: &str, port: u16) -> Vec<u8> {
    // CRLF-separated HTTP/1.0-style headers. Clients dedupe on
    // Resource-Identifier, which must match /identity's machineIdentifier.
    let node = advertisement
        .node_id
        .map(|node_id| format!("Node-Identifier: {node_id}\r\n"))
        .unwrap_or_default();
    let body = format!(
        "HTTP/1.0 200 OK\r\n\
         Content-Type: plex/media-server\r\n\
         Resource-Identifier: {}\r\n\
         {node}\
         Name: {}\r\n\
         Port: {port}\r\n\
         Version: {version}\r\n\
         Server-Class: \r\n\r\n",
        advertisement.instance_id, advertisement.name,
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
        let r = response("abc123", "den", "0.0.2", 32400);
        let text = String::from_utf8(r).expect("utf8");
        assert!(text.starts_with("HTTP/1.0 200 OK"));
        assert!(text.contains("Content-Type: plex/media-server"));
        assert!(text.contains("Resource-Identifier: abc123"));
        assert!(text.contains("Name: den"));
        assert!(text.contains("Port: 32400"));
    }

    #[test]
    fn clustered_response_keeps_logical_identity_and_adds_node_identity() {
        let response = response_for(
            &Advertisement {
                instance_id: "logical",
                name: "Living Room",
                node_id: Some("node-b"),
            },
            "0.2.0",
            32400,
        );
        let text = String::from_utf8(response).expect("utf8");
        assert!(text.contains("Resource-Identifier: logical\r\n"));
        assert!(text.contains("Node-Identifier: node-b\r\n"));
        assert!(text.contains("Name: Living Room\r\n"));
    }

    #[test]
    fn single_node_wrapper_preserves_the_original_wire_bytes() {
        let expected = b"HTTP/1.0 200 OK\r\nContent-Type: plex/media-server\r\nResource-Identifier: abc123\r\nName: den\r\nPort: 32400\r\nVersion: 0.0.2\r\nServer-Class: \r\n\r\n";
        assert_eq!(response("abc123", "den", "0.0.2", 32400), expected);
    }
}
