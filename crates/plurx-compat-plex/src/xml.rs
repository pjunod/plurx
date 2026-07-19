//! A tiny XML builder for Plex `MediaContainer` responses.
//!
//! Plex's API is attribute-heavy and shallow, so a full XML library is
//! overkill. This builds elements with attributes and nested children and
//! escapes attribute values correctly.

/// An XML element: a tag, attributes (in insertion order), and children.
pub struct Element {
    tag: String,
    attrs: Vec<(String, String)>,
    children: Vec<Element>,
}

impl Element {
    pub fn new(tag: &str) -> Self {
        Element {
            tag: tag.to_owned(),
            attrs: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Set a string attribute.
    pub fn attr(mut self, key: &str, value: impl Into<String>) -> Self {
        self.attrs.push((key.to_owned(), value.into()));
        self
    }

    /// Set an attribute only when the value is present.
    pub fn attr_opt(mut self, key: &str, value: Option<impl Into<String>>) -> Self {
        if let Some(v) = value {
            self.attrs.push((key.to_owned(), v.into()));
        }
        self
    }

    /// Set an integer attribute.
    pub fn attr_i(self, key: &str, value: i64) -> Self {
        self.attr(key, value.to_string())
    }

    pub fn attr_i_opt(self, key: &str, value: Option<i64>) -> Self {
        match value {
            Some(v) => self.attr(key, v.to_string()),
            None => self,
        }
    }

    pub fn child(mut self, child: Element) -> Self {
        self.children.push(child);
        self
    }

    pub fn children(mut self, mut kids: Vec<Element>) -> Self {
        self.children.append(&mut kids);
        self
    }

    /// Number of direct children (Plex's `size` attribute on a container).
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    fn write(&self, out: &mut String) {
        out.push('<');
        out.push_str(&self.tag);
        for (k, v) in &self.attrs {
            out.push(' ');
            out.push_str(k);
            out.push_str("=\"");
            escape_into(v, out);
            out.push('"');
        }
        if self.children.is_empty() {
            out.push_str("/>");
        } else {
            out.push('>');
            for c in &self.children {
                c.write(out);
            }
            out.push_str("</");
            out.push_str(&self.tag);
            out.push('>');
        }
    }

    /// Render a full XML document (with declaration).
    pub fn to_document(&self) -> String {
        let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        self.write(&mut out);
        out
    }
}

fn escape_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_attributes_and_children() {
        let el = Element::new("MediaContainer")
            .attr_i("size", 1)
            .attr("friendlyName", "plurx")
            .child(Element::new("Directory").attr("title", "Movies & Shows"));
        let doc = el.to_document();
        assert!(doc.contains("<MediaContainer size=\"1\" friendlyName=\"plurx\">"));
        // Ampersand in an attribute is escaped.
        assert!(doc.contains("title=\"Movies &amp; Shows\""));
        assert!(doc.contains("</MediaContainer>"));
    }

    #[test]
    fn empty_element_is_self_closing() {
        let doc = Element::new("Video").attr("k", "v").to_document();
        assert!(doc.contains("<Video k=\"v\"/>"));
    }

    #[test]
    fn attr_opt_skips_none() {
        let el = Element::new("X")
            .attr_opt("a", Some("1"))
            .attr_opt("b", None::<String>)
            .attr_i_opt("c", None);
        let doc = el.to_document();
        assert!(doc.contains("a=\"1\""));
        assert!(!doc.contains("b="));
        assert!(!doc.contains("c="));
    }
}
