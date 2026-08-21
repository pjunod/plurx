//! One registry for ebook detection and the actions every Cinema surface may claim.
//!
//! Serving original bytes is not evidence that Cinema can render them.  File DTOs,
//! clients, tests, and the documented support matrix all consume this registry so
//! adding an extension cannot accidentally turn **Open in…** into **Read**.

use std::path::Path;

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReaderAction {
    Read,
    OpenIn,
    Unavailable,
}

#[cfg(test)]
impl ReaderAction {
    fn matrix_label(self) -> &'static str {
        match self {
            Self::Read => "Read",
            Self::OpenIn => "Open in…",
            Self::Unavailable => "—",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ReaderSurfaceCapability {
    pub online: ReaderAction,
    pub offline: ReaderAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ReaderCapability {
    pub format: &'static str,
    pub web: ReaderSurfaceCapability,
    pub apple: ReaderSurfaceCapability,
    pub android: ReaderSurfaceCapability,
    pub television: ReaderSurfaceCapability,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ReaderFormat {
    pub label: &'static str,
    pub extensions: &'static [&'static str],
    #[serde(flatten)]
    pub capability: ReaderCapability,
}

const READ_ONLINE: ReaderSurfaceCapability = ReaderSurfaceCapability {
    online: ReaderAction::Read,
    offline: ReaderAction::Unavailable,
};
const READ_OFFLINE: ReaderSurfaceCapability = ReaderSurfaceCapability {
    online: ReaderAction::Read,
    offline: ReaderAction::Read,
};
const READ_ON_APPLE: ReaderSurfaceCapability = ReaderSurfaceCapability {
    online: ReaderAction::Read,
    offline: ReaderAction::Unavailable,
};
const HANDOFF: ReaderSurfaceCapability = ReaderSurfaceCapability {
    online: ReaderAction::OpenIn,
    offline: ReaderAction::Unavailable,
};
const NONE: ReaderSurfaceCapability = ReaderSurfaceCapability {
    online: ReaderAction::Unavailable,
    offline: ReaderAction::Unavailable,
};

const fn handoff(format: &'static str) -> ReaderCapability {
    ReaderCapability {
        format,
        web: HANDOFF,
        apple: HANDOFF,
        android: HANDOFF,
        television: NONE,
    }
}

pub static FORMAT_REGISTRY: &[ReaderFormat] = &[
    ReaderFormat {
        label: "EPUB",
        extensions: &["epub"],
        capability: ReaderCapability {
            format: "epub",
            web: READ_ONLINE,
            apple: READ_OFFLINE,
            android: READ_OFFLINE,
            television: NONE,
        },
    },
    ReaderFormat {
        label: "PDF",
        extensions: &["pdf"],
        capability: ReaderCapability {
            format: "pdf",
            web: HANDOFF,
            apple: READ_ON_APPLE,
            android: HANDOFF,
            television: NONE,
        },
    },
    ReaderFormat {
        label: "MOBI",
        extensions: &["mobi"],
        capability: handoff("mobi"),
    },
    ReaderFormat {
        label: "AZW / AZW3",
        extensions: &["azw", "azw3"],
        capability: handoff("azw"),
    },
    ReaderFormat {
        label: "FB2",
        extensions: &["fb2"],
        capability: handoff("fb2"),
    },
    ReaderFormat {
        label: "CBZ",
        extensions: &["cbz"],
        capability: handoff("cbz"),
    },
    ReaderFormat {
        label: "CBR",
        extensions: &["cbr"],
        capability: handoff("cbr"),
    },
];

pub fn capability(path: &Path, container: Option<&str>) -> Option<ReaderCapability> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .or(container)?;
    FORMAT_REGISTRY
        .iter()
        .find(|entry| {
            entry
                .extensions
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(extension))
        })
        .map(|entry| entry.capability)
}

#[cfg(test)]
fn support_matrix_markdown() -> String {
    let mut output = String::from(
        "| Format | Web online | Apple online / offline | Android online / offline | Television |\n\
         |---|---|---|---|---|\n",
    );
    for entry in FORMAT_REGISTRY {
        let pair = |surface: ReaderSurfaceCapability| {
            format!(
                "{} / {}",
                surface.online.matrix_label(),
                surface.offline.matrix_label()
            )
        };
        output.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            entry.label,
            entry.capability.web.online.matrix_label(),
            pair(entry.capability.apple),
            pair(entry.capability.android),
            entry.capability.television.online.matrix_label(),
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_does_not_imply_a_built_in_reader() {
        let epub = capability(Path::new("Novel.EPUB"), None).expect("EPUB");
        assert_eq!(epub.web.online, ReaderAction::Read);
        assert_eq!(epub.android.offline, ReaderAction::Read);

        for filename in [
            "paper.pdf",
            "legacy.mobi",
            "kindle.azw3",
            "story.fb2",
            "comic.cbz",
            "comic.cbr",
        ] {
            let detected = capability(Path::new(filename), None).expect(filename);
            assert_eq!(detected.web.online, ReaderAction::OpenIn, "{filename}");
            assert_eq!(
                detected.apple.offline,
                ReaderAction::Unavailable,
                "{filename}"
            );
            assert_eq!(
                detected.television.online,
                ReaderAction::Unavailable,
                "{filename}"
            );
        }
    }

    #[test]
    fn wire_actions_use_the_client_contract_spelling() {
        let epub = capability(Path::new("book.epub"), None).expect("EPUB");
        let value = serde_json::to_value(epub).expect("serialize reader capability");
        assert_eq!(value["format"], "epub");
        assert_eq!(value["web"]["online"], "read");
        assert_eq!(value["web"]["offline"], "unavailable");
        assert_eq!(value["apple"]["offline"], "read");
        assert_eq!(value["television"]["online"], "unavailable");

        let pdf = capability(Path::new("book.pdf"), None).expect("PDF");
        let value = serde_json::to_value(pdf).expect("serialize PDF capability");
        assert_eq!(value["web"]["online"], "open_in");
        assert_eq!(value["apple"]["online"], "read");
        assert_eq!(value["apple"]["offline"], "unavailable");
    }

    #[test]
    fn path_extension_wins_and_container_is_only_a_fallback() {
        assert_eq!(
            capability(Path::new("book.pdf"), Some("epub"))
                .expect("PDF")
                .format,
            "pdf"
        );
        assert_eq!(
            capability(Path::new("book"), Some("EPUB"))
                .expect("EPUB")
                .format,
            "epub"
        );
        assert!(capability(Path::new("notes.txt"), Some("epub")).is_none());
    }

    #[test]
    fn client_documentation_is_generated_from_this_registry() {
        let documentation = include_str!("../../../docs/CLIENTS.md");
        let start = documentation
            .find("<!-- reader-support:start -->")
            .expect("reader support start marker");
        let after_start = &documentation[start..];
        let table_start = after_start.find('\n').expect("table line") + 1;
        let end = after_start
            .find("<!-- reader-support:end -->")
            .expect("reader support end marker");
        assert_eq!(
            after_start[table_start..end].trim(),
            support_matrix_markdown().trim()
        );
    }
}
