//! Pre-layout map from `{% tag %}` anchor id → display label for
//! `{% tagref %}`.
//!
//! Section numbers are computed with the same counter rules as
//! `layout_heading` (enabled flag, `max_depth`, `numbered="false"`
//! opt-out), so a forward reference can still show e.g.
//! `"6.5 Manual brake release"` even when the target heading appears
//! later in the document.

use std::collections::HashMap;

use markdoc::types::{RenderableTreeNode, Scalar, Tag};

use super::block::bump_heading_counters;
use super::inline::anchor_id_attr;
use super::style::HeadingNumbering;

/// Walk `root` and build `anchor_id → "6.5 Manual brake release"` labels
/// for every `{% tag %}` declared on a heading.
pub fn build_crossref_labels(
    root: &RenderableTreeNode,
    numbering: &HeadingNumbering,
) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    let mut counters = [0u32; 6];
    walk(root, numbering, &mut counters, &mut labels);
    labels
}

fn walk(
    node: &RenderableTreeNode,
    numbering: &HeadingNumbering,
    counters: &mut [u32; 6],
    labels: &mut HashMap<String, String>,
) {
    let RenderableTreeNode::Tag(tag) = node else {
        return;
    };
    if let Some(level) = heading_level(&tag.name) {
        record_heading(tag, level, numbering, counters, labels);
    }
    for child in &tag.children {
        walk(child, numbering, counters, labels);
    }
}

fn heading_level(name: &str) -> Option<u8> {
    match name {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn record_heading(
    tag: &Tag,
    level: u8,
    numbering: &HeadingNumbering,
    counters: &mut [u32; 6],
    labels: &mut HashMap<String, String>,
) {
    let mut heading_anchor: Option<String> = None;
    let mut opt_out_numbering = false;
    for child in &tag.children {
        if let RenderableTreeNode::Tag(t) = child
            && t.name == "tag"
        {
            if heading_anchor.is_none()
                && let Some(id) = anchor_id_attr(t)
            {
                heading_anchor = Some(id);
            }
            if attr_is_false(t, "numbered") {
                opt_out_numbering = true;
            }
        }
    }

    let title = extract_heading_title(&tag.children);
    if title.is_empty() {
        return;
    }

    // Always advance counters in lock-step with layout_heading — even
    // when this heading has no `{% tag %}` — so later labels stay aligned.
    let number = if numbering.enabled && !opt_out_numbering {
        bump_heading_counters(counters, level, numbering.max_depth)
    } else {
        None
    };

    let Some(id) = heading_anchor else {
        return;
    };

    let label = match number {
        Some(n) => format!("{n}{}{title}", numbering.separator),
        None => title,
    };
    labels.insert(id, label);
}

/// Plain heading title: skip `{% tag %}` declarations, flatten nested
/// inline markup (`strong` / `em` / …) to text.
fn extract_heading_title(children: &[RenderableTreeNode]) -> String {
    let mut out = String::new();
    extract_plain(&mut out, children);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_plain(out: &mut String, children: &[RenderableTreeNode]) {
    for child in children {
        match child {
            RenderableTreeNode::Scalar(Scalar::String(s)) => out.push_str(s),
            RenderableTreeNode::Scalar(Scalar::Array(items)) => {
                for item in items {
                    match item {
                        Scalar::String(s) => out.push_str(s),
                        _ => {}
                    }
                }
            }
            RenderableTreeNode::Tag(t) if t.name == "tag" => {}
            RenderableTreeNode::Tag(t) => extract_plain(out, &t.children),
            RenderableTreeNode::Scalar(_) => {}
        }
    }
}

fn attr_is_false(tag: &Tag, key: &str) -> bool {
    match tag.attributes.get(key) {
        Some(Scalar::Boolean(b)) => !b,
        Some(Scalar::String(s)) => {
            let s = s.trim();
            s.eq_ignore_ascii_case("false") || s == "0"
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use markdoc::types::Config;
    use markdoc::{parse, transform};

    fn enabled_numbering() -> HeadingNumbering {
        HeadingNumbering {
            enabled: true,
            max_depth: 3,
            separator: " ".to_string(),
        }
    }

    fn labels_for(src: &str, numbering: HeadingNumbering) -> HashMap<String, String> {
        let doc = parse(src, None).unwrap();
        let tree = transform(&doc, &Config::default()).unwrap();
        build_crossref_labels(&tree, &numbering)
    }

    #[test]
    fn numbered_heading_tag_gets_section_label() {
        let src = r#"
# Usage

## Manual brake release {% tag "manual_brake_release_switch" /%}

See later.
"#;
        let labels = labels_for(src, enabled_numbering());
        assert_eq!(
            labels.get("manual_brake_release_switch").map(String::as_str),
            Some("1.1 Manual brake release")
        );
    }

    #[test]
    fn unnumbered_opt_out_uses_title_only() {
        let src = r#"
# Intro {% tag id="intro" numbered="false" /%}

# Usage {% tag id="usage" /%}
"#;
        let labels = labels_for(src, enabled_numbering());
        assert_eq!(labels.get("intro").map(String::as_str), Some("Intro"));
        // Intro opted out so it did not consume "1"; Usage is "1".
        assert_eq!(labels.get("usage").map(String::as_str), Some("1 Usage"));
    }

    #[test]
    fn primary_shorthand_with_numbered_false() {
        let src = r#"# Intro {% tag "intro" numbered="false" /%}"#;
        let labels = labels_for(src, enabled_numbering());
        assert_eq!(labels.get("intro").map(String::as_str), Some("Intro"));
    }

    #[test]
    fn numbering_disabled_uses_title_only() {
        let src = r#"# Manual brake release {% tag id="x" /%}"#;
        let mut numbering = enabled_numbering();
        numbering.enabled = false;
        let labels = labels_for(src, numbering);
        assert_eq!(
            labels.get("x").map(String::as_str),
            Some("Manual brake release")
        );
    }
}
