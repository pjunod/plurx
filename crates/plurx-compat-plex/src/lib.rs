//! Plex Media Server API compatibility helpers.
//!
//! Tier 1 target (docs/CLIENTS.md §3): direct-connect clients — Composite for
//! Kodi, PlexKodiConnect, python-plexapi tooling, Home Assistant. This crate
//! is the pure Plex-protocol layer (XML `MediaContainer` building, domain →
//! Plex mapping, GDM discovery); plurxd mounts the HTTP routes and wires them
//! to plurx-core services. It never contacts plex.tv (REQ-PLEX-3).

pub mod gdm;
pub mod map;
pub mod xml;

pub use xml::Element;

/// A friendly product string advertised to clients.
pub const PRODUCT: &str = "plurx";

/// Build the `/identity` container (unauthenticated liveness + identity).
pub fn identity_container(machine_identifier: &str, version: &str) -> Element {
    Element::new("MediaContainer")
        .attr_i("size", 0)
        .attr("claimed", "0")
        .attr("machineIdentifier", machine_identifier.to_owned())
        .attr("version", version.to_owned())
}

/// Build the root `/` capabilities container.
pub fn root_container(machine_identifier: &str, name: &str, version: &str) -> Element {
    Element::new("MediaContainer")
        .attr_i("size", 3)
        .attr("friendlyName", name.to_owned())
        .attr("machineIdentifier", machine_identifier.to_owned())
        .attr("version", version.to_owned())
        .attr("platform", std::env::consts::OS)
        .attr("product", PRODUCT)
        .attr("allowSync", "0")
        .attr("multiuser", "1")
        .child(
            Element::new("Directory")
                .attr("key", "/library/sections")
                .attr("title", "Library"),
        )
        .child(
            Element::new("Directory")
                .attr("key", "/hubs")
                .attr("title", "Hubs"),
        )
        .child(
            Element::new("Directory")
                .attr("key", "/search")
                .attr("title", "Search"),
        )
}

/// Wrap child elements in a `MediaContainer` with an accurate `size`.
pub fn container(children: Vec<Element>) -> Element {
    let size = children.len() as i64;
    Element::new("MediaContainer")
        .attr_i("size", size)
        .children(children)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_has_machine_id() {
        let doc = identity_container("m-123", "0.0.2").to_document();
        assert!(doc.contains("machineIdentifier=\"m-123\""));
        assert!(doc.contains("<MediaContainer"));
    }

    #[test]
    fn root_lists_library_section() {
        let doc = root_container("m-123", "den", "0.0.2").to_document();
        assert!(doc.contains("friendlyName=\"den\""));
        assert!(doc.contains("key=\"/library/sections\""));
    }

    #[test]
    fn container_counts_children() {
        let c = container(vec![Element::new("Video"), Element::new("Video")]);
        assert!(c.to_document().contains("size=\"2\""));
    }
}
