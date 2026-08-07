//! Collect inline-level content from a tag's children: produces a flat
//! string plus a list of `InlineRange`s describing where bold/italic/
//! strikethrough apply.

use std::collections::HashMap;

use markdoc::types::{RenderableTreeNode, Scalar};

#[derive(Debug, Clone, Copy)]
pub enum InlineProp {
    Bold,
    Italic,
    Strikethrough,
    /// Inline code span (`` `like_this` ``) — rendered in the monospace
    /// family so it reads distinctly from the surrounding prose.
    Code,
    /// Underline for a link. The colour comes from the text brush (link
    /// text is already tinted via `Color`); only the configured thickness
    /// is carried here. Vertical position is font-derived at draw time.
    Underline {
        thickness: f32,
    },
    /// Override the foreground colour for this range — used by the
    /// code-block syntax highlighter so that keywords/strings/comments
    /// can be tinted independently of the body text colour.
    Color(krilla::color::rgb::Color),
}

#[derive(Debug, Clone)]
pub struct InlineRange {
    pub start: usize,
    pub end: usize,
    pub prop: InlineProp,
}

/// A hyperlink covering bytes `[start, end)` in the collected text.
/// `href` may be a relative URL, an absolute URL, an `#anchor`, etc. —
/// the renderer passes it to krilla as a URI action verbatim.
#[derive(Debug, Clone)]
pub struct LinkRange {
    pub start: usize,
    pub end: usize,
    pub href: String,
    /// Optional title attribute on the link, used as alt text on the PDF
    /// annotation when present.
    pub title: Option<String>,
}

/// A mid-paragraph anchor declaration `{% tag id="X" %}` — the byte
/// offset records where in the collected text the tag appeared, used
/// later to compute its (line, y) for the PDF destination.
#[derive(Debug, Clone)]
pub struct MidAnchor {
    pub byte_offset: usize,
    pub id: String,
}

/// One `{% footnote %}…{% /footnote %}` call site recorded during
/// inline collection. `byte_offset` points at the FIRST byte of the
/// superscript call mark inside `Inlines::text`; `number` is the
/// 1-based footnote number assigned at collection time so the body
/// in the global registry can be matched at pagination time.
#[derive(Debug, Clone)]
pub struct FootnoteCall {
    pub byte_offset: usize,
    pub number: u32,
}

/// Collected inline content from a tag's children: text plus the style
/// ranges, link ranges, mid-paragraph anchor declarations, and
/// footnote call sites found within it.
pub struct Inlines {
    pub text: String,
    pub style_ranges: Vec<InlineRange>,
    pub links: Vec<LinkRange>,
    pub mid_anchors: Vec<MidAnchor>,
    pub footnote_calls: Vec<FootnoteCall>,
}

impl Inlines {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            style_ranges: Vec::new(),
            links: Vec::new(),
            mid_anchors: Vec::new(),
            footnote_calls: Vec::new(),
        }
    }

    /// Collect inline content from `children`. `footnotes` is the
    /// document-wide footnote-body registry: every `{% footnote %}`
    /// encountered appends its body and the call mark gets the new
    /// 1-based index. Pass a throwaway buffer at sites where footnotes
    /// shouldn't survive (captions, table cells, headings inside ToC,
    /// or measurement-only passes).
    ///
    /// `crossref_labels` maps `{% tag %}` ids to display text for
    /// `{% tagref %}` (e.g. `"6.5 Manual brake release"`). When `None`
    /// or missing an id, the raw anchor id is used as link text.
    pub fn from(children: &[RenderableTreeNode], footnotes: &mut Vec<String>) -> Self {
        Self::from_with_labels(children, footnotes, None)
    }

    pub fn from_with_labels(
        children: &[RenderableTreeNode],
        footnotes: &mut Vec<String>,
        crossref_labels: Option<&HashMap<String, String>>,
    ) -> Self {
        let mut i = Self::new();
        collect_into(&mut i, children, footnotes, crossref_labels);
        i
    }
}

/// Is the href something a PDF viewer can actually navigate to?
///
/// - `#anchor` — internal document destination, resolved by the
///   emit layer; always OK.
/// - `scheme:...` matching RFC 3986 (`http`, `https`, `mailto`,
///   `tel`, `file`, …) — has a scheme, viewer can dispatch.
/// - Anything else (`coverpage.style.toml`, `./foo.md`, `../bar`)
///   is a relative path and PDF viewers treat the `/URI` action as
///   non-navigable. Triggers the relative-href warning.
fn is_navigable_uri(href: &str) -> bool {
    if let Some(first) = href.chars().next()
        && first == '#'
    {
        return true;
    }
    // RFC 3986 scheme: ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) followed by ':'
    let mut chars = href.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    for c in chars {
        match c {
            ':' => return true,
            c if c.is_ascii_alphanumeric() => continue,
            '+' | '-' | '.' => continue,
            _ => return false,
        }
    }
    false
}

/// Render an unsigned integer as the corresponding Unicode
/// superscript characters (e.g. `12 → "¹²"`). Used as the inline call
/// mark for a footnote.
pub fn superscript_number(n: u32) -> String {
    const MAP: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
    let s = n.to_string();
    s.chars()
        .map(|c| c.to_digit(10).map(|d| MAP[d as usize]).unwrap_or(c))
        .collect()
}

/// Extract the anchor id from a `<tag>` or `<tagref>` Tag node.
///
/// Two accepted spellings:
///   - `{% tag id="foo" /%}`    — keyed attribute, the canonical form
///   - `{% tag "foo" /%}`       — primary attribute (Markdoc shorthand)
///
/// The earlier `{% tag="foo" /%}` typo-form was dropped — it's just a
/// noisier spelling of the primary shorthand and the parser now handles
/// `"foo"` cleanly without renderer post-processing.
pub fn anchor_id_attr(tag: &markdoc::types::Tag) -> Option<String> {
    if let Some(Scalar::String(s)) = tag.attributes.get("id")
        && !s.is_empty()
    {
        return Some(s.clone());
    }
    if let Some(Scalar::String(s)) = tag.attributes.get("primary")
        && !s.is_empty()
    {
        return Some(s.clone());
    }
    None
}

/// Extract the cross-document target id from a `<tagref>` Tag node.
/// `doc="<id>"` selects which Adeptus document the `id="<anchor>"`
/// lives in. Empty / absent attribute means "intra-document reference"
/// and the caller falls back to plain `anchor_id_attr` resolution.
pub fn doc_attr(tag: &markdoc::types::Tag) -> Option<String> {
    if let Some(Scalar::String(s)) = tag.attributes.get("doc")
        && !s.is_empty()
    {
        return Some(s.clone());
    }
    None
}

/// Backwards-compatible helper for callers that only want the text +
/// style ranges (no link tracking). `footnotes` is the registry —
/// see [`Inlines::from`] for the same caveats.
pub fn collect_inlines(
    text: &mut String,
    ranges: &mut Vec<InlineRange>,
    children: &[RenderableTreeNode],
    footnotes: &mut Vec<String>,
) {
    let mut tmp = Inlines {
        text: std::mem::take(text),
        style_ranges: std::mem::take(ranges),
        links: Vec::new(),
        mid_anchors: Vec::new(),
        footnote_calls: Vec::new(),
    };
    collect_into(&mut tmp, children, footnotes, None);
    *text = tmp.text;
    *ranges = tmp.style_ranges;
}

fn collect_into(
    out: &mut Inlines,
    children: &[RenderableTreeNode],
    footnotes: &mut Vec<String>,
    crossref_labels: Option<&HashMap<String, String>>,
) {
    for child in children {
        match child {
            RenderableTreeNode::Scalar(Scalar::String(s)) => out.text.push_str(s),
            RenderableTreeNode::Scalar(Scalar::Array(arr)) => {
                for item in arr {
                    if let Scalar::String(s) = item {
                        out.text.push_str(s);
                    }
                }
            }
            RenderableTreeNode::Scalar(_) => {}
            RenderableTreeNode::Tag(t) => {
                let prop = match t.name.as_str() {
                    "strong" => Some(InlineProp::Bold),
                    "em" => Some(InlineProp::Italic),
                    "s" | "strikethrough" => Some(InlineProp::Strikethrough),
                    "code" => Some(InlineProp::Code),
                    "softbreak" => {
                        // CommonMark soft break (a single newline
                        // within a paragraph). Emit a space so words
                        // either side stay separated; let parley pick
                        // the wrap point.
                        out.text.push(' ');
                        collect_into(out, &t.children, footnotes, crossref_labels);
                        continue;
                    }
                    "br" | "hardbreak" => {
                        // CommonMark hard break (two trailing spaces or
                        // backslash). Force a line break — parley treats
                        // U+000A as a mandatory break.
                        out.text.push('\n');
                        collect_into(out, &t.children, footnotes, crossref_labels);
                        continue;
                    }
                    "a" => {
                        // Capture the link's byte range and href.
                        let href = match t.attributes.get("href") {
                            Some(Scalar::String(s)) => Some(s.clone()),
                            _ => None,
                        };
                        let title = match t.attributes.get("title") {
                            Some(Scalar::String(s)) if !s.is_empty() => Some(s.clone()),
                            _ => None,
                        };
                        let start = out.text.len();
                        collect_into(out, &t.children, footnotes, crossref_labels);
                        let end = out.text.len();
                        if let Some(href) = href
                            && end > start
                        {
                            // Warn on schemeless URIs — PDF viewers
                            // don't navigate relative paths, so the
                            // resulting annotation is inert. Authors
                            // either want an absolute URL or an
                            // intra-document `#anchor`.
                            if !is_navigable_uri(&href) {
                                let preview: String =
                                    out.text[start..end].chars().take(40).collect();
                                eprintln!(
                                    "warning: link href {href:?} has no URL scheme — \
                                     PDF viewers won't follow it (link text: {preview:?})"
                                );
                            }
                            out.links.push(LinkRange {
                                start,
                                end,
                                href,
                                title,
                            });
                        }
                        continue;
                    }
                    "footnote" => {
                        // Allocate the next sequential number, capture
                        // the body as plain text into the registry,
                        // and inject a Unicode superscript call mark
                        // at the current position. Nested footnotes
                        // are flattened (their bodies merge into the
                        // outer footnote text).
                        let number = (footnotes.len() as u32) + 1;
                        // Reserve the slot up front so any inner
                        // footnotes get higher numbers; then fill in
                        // the body once collected.
                        footnotes.push(String::new());
                        let mut body = Inlines::new();
                        collect_into(&mut body, &t.children, footnotes, crossref_labels);
                        let body_text = body.text.trim().to_string();
                        if let Some(slot) = footnotes.get_mut((number - 1) as usize) {
                            *slot = body_text;
                        }
                        let mark = superscript_number(number);
                        let call_offset = out.text.len();
                        out.text.push_str(&mark);
                        out.footnote_calls.push(FootnoteCall {
                            byte_offset: call_offset,
                            number,
                        });
                        continue;
                    }
                    "tagref" => {
                        // Cross-reference. Two flavours:
                        //
                        //   {% tagref id="X" /%}            — intra-doc.
                        //     Resolved to a PDF GoTo destination at emit
                        //     time via the anchor map. Link text is the
                        //     precomputed section label when available
                        //     (e.g. "6.5 Manual brake release"), else
                        //     the raw anchor id.
                        //
                        //   {% tagref doc="D" id="X" /%}    — cross-doc.
                        //     Adeptus is meant to rewrite this BEFORE
                        //     handing the source to Scriptor (into either
                        //     a normal `[text](https://…)` external URL
                        //     or an intra-bundle `{% tagref id="…" /%}`).
                        //     During local iteration nothing rewrites it,
                        //     so we render a visibly-degraded placeholder
                        //     `[doc#anchor]` with no annotation — loud
                        //     enough that writers spot unresolved refs in
                        //     a draft preview without anything failing.
                        //     See CROSS_DOC.md for the full contract.
                        let doc = doc_attr(t);
                        let id = anchor_id_attr(t);
                        match (doc, id) {
                            (Some(doc), Some(id)) => {
                                let placeholder = format!("[{doc}#{id}]");
                                out.text.push_str(&placeholder);
                            }
                            (Some(doc), None) => {
                                let placeholder = format!("[{doc}#?]");
                                out.text.push_str(&placeholder);
                            }
                            (None, Some(id)) => {
                                let label = crossref_labels
                                    .and_then(|m| m.get(&id))
                                    .cloned()
                                    .unwrap_or_else(|| id.clone());
                                let start = out.text.len();
                                out.text.push_str(&label);
                                let end = out.text.len();
                                out.links.push(LinkRange {
                                    start,
                                    end,
                                    href: format!("#{id}"),
                                    title: None,
                                });
                            }
                            (None, None) => {
                                // No id, no doc — drop silently.
                            }
                        }
                        continue;
                    }
                    "tag" => {
                        // `{% tag id="X" %}` is an anchor declaration.
                        // Headings strip these before reaching the
                        // inline collector. When a tag survives to here
                        // it's a mid-paragraph anchor — record its
                        // byte offset so the renderer can resolve the
                        // (page, y) by walking the parley layout.
                        if let Some(id) = anchor_id_attr(t) {
                            out.mid_anchors.push(MidAnchor {
                                byte_offset: out.text.len(),
                                id,
                            });
                        }
                        continue;
                    }
                    "chip" => {
                        // Inline colour chip — the inline sibling of the
                        // block `{% swatch %}`:
                        //   {% chip color="#ff0000" /%} Red
                        //   {% chip color="#0a0" shape="square" /%} Ready
                        // Self-closing, so it injects a filled marker glyph
                        // (● default, ■ for `shape="square"`) tinted with the
                        // colour. Solid only — gradients are the block bar's
                        // job. A missing / unparsable colour leaves the glyph
                        // in the default text colour.
                        let glyph = match t.attributes.get("shape") {
                            Some(Scalar::String(s)) if s.trim().eq_ignore_ascii_case("square") => {
                                '■'
                            }
                            _ => '●',
                        };
                        let color = match t.attributes.get("color") {
                            Some(Scalar::String(s)) => parse_css_color(s),
                            _ => None,
                        };
                        let start = out.text.len();
                        out.text.push(glyph);
                        let end = out.text.len();
                        if let Some(c) = color {
                            out.style_ranges.push(InlineRange {
                                start,
                                end,
                                prop: InlineProp::Color(c),
                            });
                        }
                        continue;
                    }
                    "color" | "c" => {
                        // Inline colour span:
                        //   {% color value="#d21e1e" %}red text{% /color %}
                        // Maps the wrapped text to an `InlineProp::Color`
                        // range; an unparsable / missing `value` leaves the
                        // text in the default colour.
                        match t.attributes.get("value") {
                            Some(Scalar::String(s)) => parse_css_color(s).map(InlineProp::Color),
                            _ => None,
                        }
                    }
                    "ref" => {
                        // Inline cross-document reference:
                        //   See {% ref document="4100237" %} for details.
                        //   {% ref document="4100237" label="the migration guide" %}
                        // Adeptus resolves it to the target's title + URL. Here,
                        // without Adeptus, render the `label` (or "doc <number>")
                        // as a placeholder link to a `flux:` URI — it styles like
                        // any other link; the real target is resolved upstream.
                        let attr = |k: &str| match t.attributes.get(k) {
                            Some(Scalar::String(s)) if !s.trim().is_empty() => {
                                Some(s.trim().to_string())
                            }
                            Some(Scalar::Number(n)) if n.fract() == 0.0 => {
                                Some(format!("{}", *n as i64))
                            }
                            Some(Scalar::Number(n)) => Some(n.to_string()),
                            _ => None,
                        };
                        let doc = attr("document");
                        if let Some(target) = doc.clone().or_else(|| attr("uuid")) {
                            let label = attr("label").unwrap_or_else(|| format!("doc {target}"));
                            let href = if doc.is_some() {
                                format!("flux:document/{target}")
                            } else {
                                format!("flux:uuid/{target}")
                            };
                            let start = out.text.len();
                            out.text.push_str(&label);
                            let end = out.text.len();
                            out.links.push(LinkRange {
                                start,
                                end,
                                href,
                                title: None,
                            });
                        }
                        continue;
                    }
                    _ => None,
                };
                let start = out.text.len();
                collect_into(out, &t.children, footnotes, crossref_labels);
                let end = out.text.len();
                if let Some(p) = prop
                    && end > start
                {
                    out.style_ranges.push(InlineRange {
                        start,
                        end,
                        prop: p,
                    });
                }
            }
        }
    }
}

/// Parse a CSS-ish colour for the `{% color %}` inline tag: a `#rgb` or
/// `#rrggbb` hex string, or one of a small set of named colours. Returns
/// `None` for anything unrecognised. Also reused by the `{% table %}`
/// per-table colour attributes.
pub(super) fn parse_css_color(s: &str) -> Option<krilla::color::rgb::Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let (r, g, b) = match hex.len() {
            3 => {
                let d = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|n| n * 17);
                (d(0)?, d(1)?, d(2)?)
            }
            6 => {
                let d = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
                (d(0)?, d(2)?, d(4)?)
            }
            _ => return None,
        };
        return Some(krilla::color::rgb::Color::new(r, g, b));
    }
    let (r, g, b) = match s.to_ascii_lowercase().as_str() {
        "red" => (204, 0, 0),
        "green" => (0, 128, 0),
        "blue" => (0, 0, 204),
        "orange" => (255, 120, 0),
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        _ => return None,
    };
    Some(krilla::color::rgb::Color::new(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_and_named_colors() {
        use krilla::color::rgb::Color;
        assert_eq!(parse_css_color("#d21e1e"), Some(Color::new(210, 30, 30)));
        assert_eq!(parse_css_color("#f00"), Some(Color::new(255, 0, 0)));
        assert_eq!(parse_css_color(" red "), Some(Color::new(204, 0, 0)));
        assert_eq!(parse_css_color("not-a-color"), None);
        assert_eq!(parse_css_color("#12"), None);
    }

    #[test]
    fn navigable_intra_doc_anchor() {
        // `#anchor` is always navigable — resolved by the emit layer
        // into a GoTo destination on the anchor's page.
        assert!(is_navigable_uri("#section"));
        assert!(is_navigable_uri("#"));
        assert!(is_navigable_uri("#a-b_c.123"));
    }

    #[test]
    fn navigable_standard_schemes() {
        // Anything with an RFC 3986 scheme followed by `:` is
        // navigable from the renderer's perspective — the viewer
        // decides what it actually does with the URI.
        assert!(is_navigable_uri("http://example.com"));
        assert!(is_navigable_uri("https://example.com/path?q=1"));
        assert!(is_navigable_uri("file:///etc/hosts"));
        assert!(is_navigable_uri("mailto:alice@example.com"));
        assert!(is_navigable_uri("tel:+15551234"));
        assert!(is_navigable_uri("ftp://ftp.example.com/"));
    }

    #[test]
    fn navigable_custom_scheme() {
        // Application-specific schemes are fine — viewers may not
        // dispatch them, but the renderer can't know that and
        // shouldn't warn on syntactically-valid schemes.
        assert!(is_navigable_uri("arca://abc-def"));
        assert!(is_navigable_uri("vscode://file/foo"));
        // Scheme allows `+`, `-`, `.` after the leading ALPHA.
        assert!(is_navigable_uri("git+ssh://host/repo"));
        assert!(is_navigable_uri("foo.bar://baz"));
    }

    #[test]
    fn schemeless_relative_paths_are_dead() {
        // These are the cases that trigger the renderer's
        // "PDF viewers won't follow it" warning.
        assert!(!is_navigable_uri("coverpage.style.toml"));
        assert!(!is_navigable_uri("./foo.md"));
        assert!(!is_navigable_uri("../bar"));
        assert!(!is_navigable_uri("/absolute/path"));
        assert!(!is_navigable_uri("just-a-word"));
    }

    #[test]
    fn schemes_must_start_with_alpha() {
        // RFC 3986: scheme MUST begin with an ASCII letter. A leading
        // digit / punctuation / non-ASCII char isn't a valid scheme.
        assert!(!is_navigable_uri("123:abc"));
        assert!(!is_navigable_uri("-foo:bar"));
        assert!(!is_navigable_uri(":colon-leader"));
        assert!(!is_navigable_uri(""));
    }

    #[test]
    fn scheme_must_actually_have_a_colon() {
        // "Looks like a scheme prefix but no colon" — still relative.
        // Otherwise every all-lowercase-no-punct word would qualify.
        assert!(!is_navigable_uri("http"));
        assert!(!is_navigable_uri("about"));
    }
}
