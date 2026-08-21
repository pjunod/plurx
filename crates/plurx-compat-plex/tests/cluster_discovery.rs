use plurx_compat_plex::gdm::{response_for, Advertisement};

#[test]
fn clustered_gdm_keeps_logical_identity_and_adds_the_node() {
    let response = response_for(
        &Advertisement {
            instance_id: "logical-server",
            name: "Living Room",
            node_id: Some("node-b"),
        },
        "0.2.0",
        32400,
    );
    let response = String::from_utf8(response).expect("GDM response is UTF-8");
    assert!(response.contains("Resource-Identifier: logical-server\r\n"));
    assert!(response.contains("Node-Identifier: node-b\r\n"));
    assert!(response.contains("Name: Living Room\r\n"));
}

#[test]
fn legacy_single_node_gdm_is_byte_for_byte_unchanged() {
    let expected = b"HTTP/1.0 200 OK\r\nContent-Type: plex/media-server\r\nResource-Identifier: abc123\r\nName: den\r\nPort: 32400\r\nVersion: 0.0.2\r\nServer-Class: \r\n\r\n";
    assert_eq!(
        plurx_compat_plex::gdm::response("abc123", "den", "0.0.2", 32400),
        expected
    );
}
