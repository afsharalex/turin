use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::cli::{AnchorKind, StopArgs};
use crate::util::{die, toml_quote};

/// Frontmatter shape, as parsed from a stop file.
#[derive(Deserialize, Debug)]
pub struct StopFrontmatter {
    pub file: String,
    pub anchor: Anchor,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub highlight: Option<toml::Value>,
}

#[derive(Deserialize, Debug)]
pub struct Anchor {
    pub kind: String,
    #[serde(default)]
    pub value: Option<toml::Value>,
    #[serde(default)]
    pub query: Option<String>,
}

impl StopFrontmatter {
    /// Compact human-readable summary of the anchor for tabular display.
    pub fn anchor_display(&self) -> String {
        match self.anchor.kind.as_str() {
            "line" => format!(
                "line {}",
                self.anchor
                    .value
                    .as_ref()
                    .and_then(|v| v.as_integer())
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into())
            ),
            "pattern" => format!(
                "pattern /{}/",
                self.anchor
                    .value
                    .as_ref()
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
            ),
            "treesitter" => format!("ts {}", self.anchor.query.as_deref().unwrap_or("?")),
            other => other.to_string(),
        }
    }
}

/// Build the full text of a stop file (frontmatter + body).
pub fn build_content(stop: &StopArgs) -> String {
    let mut out = String::from("---\n");

    if let Some(id) = &stop.id {
        out.push_str(&format!("id = {}\n", toml_quote(id)));
    }

    let file = stop.file.as_ref().expect("validated by require_complete");
    out.push_str(&format!("file = {}\n", toml_quote(&file.to_string_lossy())));

    let kind = stop.anchor_kind.expect("validated by require_complete");
    let value = stop
        .anchor
        .as_deref()
        .expect("validated by require_complete");
    let anchor_line = match kind {
        AnchorKind::Line => format!("anchor = {{ kind = \"line\", value = {} }}\n", value),
        AnchorKind::Pattern => format!(
            "anchor = {{ kind = \"pattern\", value = {} }}\n",
            toml_quote(value)
        ),
        AnchorKind::Treesitter => format!(
            "anchor = {{ kind = \"treesitter\", query = {} }}\n",
            toml_quote(value)
        ),
    };
    out.push_str(&anchor_line);

    if let Some(title) = &stop.title {
        out.push_str(&format!("title = {}\n", toml_quote(title)));
    }
    if let Some(n) = stop.highlight_lines {
        out.push_str(&format!("highlight = {{ lines = {} }}\n", n));
    }

    out.push_str("---\n\n");

    let body = if let Some(b) = &stop.body {
        b.clone()
    } else if let Some(p) = &stop.body_file {
        fs::read_to_string(p).unwrap_or_else(|e| die(format!("reading {}: {}", p.display(), e)))
    } else {
        String::new()
    };
    out.push_str(&body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Write a stop file under `turin_dir`, returning the filename written.
pub fn write_file(turin_dir: &Path, stop: &StopArgs) -> String {
    let filename = format!("{}.md", stop.slug());
    let path: PathBuf = turin_dir.join(&filename);
    if path.exists() {
        die(format!("{} already exists", path.display()));
    }
    fs::write(&path, build_content(stop))
        .unwrap_or_else(|e| die(format!("writing {}: {}", path.display(), e)));
    filename
}

/// Parse the TOML frontmatter from a stop file.
pub fn parse_frontmatter(path: &Path) -> StopFrontmatter {
    parse(path).0
}

/// Read a stop file fully, returning both frontmatter and body.
pub fn parse(path: &Path) -> (StopFrontmatter, String) {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| die(format!("reading {}: {}", path.display(), e)));
    let (frontmatter, body) = split_frontmatter(&text).unwrap_or_else(|| {
        die(format!(
            "{}: missing `---` frontmatter delimiters",
            path.display()
        ))
    });
    let fm: StopFrontmatter = toml::from_str(frontmatter)
        .unwrap_or_else(|e| die(format!("parsing frontmatter in {}: {}", path.display(), e)));
    (fm, body.to_string())
}

/// Split a stop file's text into (frontmatter, body) by the leading `---` block.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    if let Some(end) = rest.find("\n---\n") {
        Some((&rest[..end], &rest[end + 5..]))
    } else if let Some(end) = rest.find("\n---\r\n") {
        Some((&rest[..end], &rest[end + 6..]))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{AnchorKind, StopArgs};

    fn pattern_stop() -> StopArgs {
        StopArgs {
            file: Some(PathBuf::from("src/foo.rs")),
            anchor_kind: Some(AnchorKind::Pattern),
            anchor: Some("fn handler".into()),
            title: Some("Handler".into()),
            ..Default::default()
        }
    }

    #[test]
    fn build_content_writes_pattern_anchor_with_quoted_value() {
        let c = build_content(&pattern_stop());
        assert!(c.contains(r#"anchor = { kind = "pattern", value = "fn handler" }"#));
    }

    #[test]
    fn build_content_writes_line_anchor_with_unquoted_integer() {
        let s = StopArgs {
            anchor_kind: Some(AnchorKind::Line),
            anchor: Some("42".into()),
            ..pattern_stop()
        };
        let c = build_content(&s);
        assert!(c.contains(r#"anchor = { kind = "line", value = 42 }"#));
    }

    #[test]
    fn build_content_writes_treesitter_anchor_with_query_field() {
        let s = StopArgs {
            anchor_kind: Some(AnchorKind::Treesitter),
            anchor: Some("(function_item)".into()),
            ..pattern_stop()
        };
        let c = build_content(&s);
        assert!(c.contains(r#"anchor = { kind = "treesitter", query = "(function_item)" }"#));
    }

    #[test]
    fn build_content_omits_optional_fields_when_unset() {
        let s = StopArgs {
            title: None,
            id: None,
            highlight_lines: None,
            ..pattern_stop()
        };
        let c = build_content(&s);
        assert!(!c.contains("title ="));
        assert!(!c.contains("id ="));
        assert!(!c.contains("highlight"));
    }

    #[test]
    fn build_content_includes_inline_body_with_trailing_newline() {
        let s = StopArgs {
            body: Some("hello world".into()),
            ..pattern_stop()
        };
        let c = build_content(&s);
        assert!(c.contains("\n---\n\nhello world\n"));
        assert!(c.ends_with('\n'));
    }

    #[test]
    fn build_content_starts_and_ends_frontmatter_with_dashes() {
        let c = build_content(&pattern_stop());
        assert!(c.starts_with("---\n"));
        assert!(c.contains("\n---\n\n"));
    }

    #[test]
    fn split_frontmatter_basic() {
        let text = "---\nfoo = 1\n---\nbody text\n";
        let (fm, body) = split_frontmatter(text).unwrap();
        assert_eq!(fm, "foo = 1");
        assert_eq!(body, "body text\n");
    }

    #[test]
    fn split_frontmatter_returns_none_without_leading_dashes() {
        assert!(split_frontmatter("just a body").is_none());
    }

    #[test]
    fn split_frontmatter_returns_none_without_closing_dashes() {
        assert!(split_frontmatter("---\nfoo = 1\nno close\n").is_none());
    }

    #[test]
    fn split_frontmatter_handles_crlf_line_endings() {
        let text = "---\r\nfoo = 1\r\n---\r\nbody\r\n";
        let (fm, body) = split_frontmatter(text).unwrap();
        assert_eq!(fm, "foo = 1\r");
        assert_eq!(body, "body\r\n");
    }

    #[test]
    fn write_file_then_parse_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let filename = write_file(dir.path(), &pattern_stop());
        assert_eq!(filename, "handler.md");

        let parsed = parse_frontmatter(&dir.path().join(&filename));
        assert_eq!(parsed.file, "src/foo.rs");
        assert_eq!(parsed.anchor.kind, "pattern");
        assert_eq!(parsed.title.as_deref(), Some("Handler"));
        assert_eq!(
            parsed.anchor.value.as_ref().and_then(|v| v.as_str()),
            Some("fn handler")
        );
    }

    fn fm(kind: &str, value: Option<toml::Value>, query: Option<&str>) -> StopFrontmatter {
        StopFrontmatter {
            file: "x".into(),
            anchor: Anchor {
                kind: kind.into(),
                value,
                query: query.map(|s| s.to_string()),
            },
            title: None,
            highlight: None,
        }
    }

    #[test]
    fn anchor_display_pattern() {
        let f = fm("pattern", Some(toml::Value::String("fn foo".into())), None);
        assert_eq!(f.anchor_display(), "pattern /fn foo/");
    }

    #[test]
    fn anchor_display_line() {
        let f = fm("line", Some(toml::Value::Integer(42)), None);
        assert_eq!(f.anchor_display(), "line 42");
    }

    #[test]
    fn anchor_display_treesitter() {
        let f = fm("treesitter", None, Some("(fn)"));
        assert_eq!(f.anchor_display(), "ts (fn)");
    }
}
