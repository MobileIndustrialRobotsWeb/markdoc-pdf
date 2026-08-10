//! Document style spec.
//!
//! Loaded either from a TOML file (`Style::from_toml_str` /
//! `Style::from_toml_file`) or built from the in-code `Default` impl. All
//! fields are `#[serde(default)]` so partial overrides are easy: a TOML
//! file can specify just the page size and inherit everything else.

use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct ColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl ColorRgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl From<ColorRgb> for krilla::color::rgb::Color {
    fn from(c: ColorRgb) -> Self {
        krilla::color::rgb::Color::new(c.r, c.g, c.b)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HeadingStyle {
    pub font_size: f32,
    pub font_weight: f32,
    pub space_before: f32,
    pub space_after: f32,
    /// Text colour for this heading level. Defaults to the document
    /// body `text_color` when unset; set it (e.g. to a brand colour)
    /// to tint headings independently of body text.
    pub color: Option<ColorRgb>,
}

impl Default for HeadingStyle {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            font_weight: 700.0,
            space_before: 12.0,
            space_after: 6.0,
            color: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HeadingStyles {
    pub h1: HeadingStyle,
    pub h2: HeadingStyle,
    pub h3: HeadingStyle,
    pub h4: HeadingStyle,
    pub h5: HeadingStyle,
    pub h6: HeadingStyle,
}

impl Default for HeadingStyles {
    fn default() -> Self {
        Self {
            h1: HeadingStyle {
                font_size: 26.0,
                font_weight: 700.0,
                space_before: 18.0,
                space_after: 12.0,
                color: None,
            },
            h2: HeadingStyle {
                font_size: 21.0,
                font_weight: 700.0,
                space_before: 16.0,
                space_after: 10.0,
                color: None,
            },
            h3: HeadingStyle {
                font_size: 17.0,
                font_weight: 700.0,
                space_before: 14.0,
                space_after: 8.0,
                color: None,
            },
            h4: HeadingStyle {
                font_size: 14.0,
                font_weight: 700.0,
                space_before: 12.0,
                space_after: 6.0,
                color: None,
            },
            h5: HeadingStyle {
                font_size: 12.0,
                font_weight: 700.0,
                space_before: 10.0,
                space_after: 6.0,
                color: None,
            },
            h6: HeadingStyle {
                font_size: 11.0,
                font_weight: 700.0,
                space_before: 10.0,
                space_after: 4.0,
                color: None,
            },
        }
    }
}

impl HeadingStyles {
    pub fn for_level(&self, level: u8) -> &HeadingStyle {
        match level {
            1 => &self.h1,
            2 => &self.h2,
            3 => &self.h3,
            4 => &self.h4,
            5 => &self.h5,
            _ => &self.h6,
        }
    }
}

/// Automatic section numbering for headings (`1`, `1.1`, `1.1.1`, …).
///
/// Off by default — themes that don't opt in render headings exactly as
/// before. When enabled, the computed prefix is baked into the heading
/// text itself, so it flows automatically into the visible heading, the
/// running header (`{chapter}` / `{section}`), and the table of contents.
///
/// Only levels `1..=max_depth` are numbered; deeper headings render
/// unprefixed (a common convention — e.g. number chapters/sections but
/// leave sub-sub-subsections plain). An individual heading opts out by
/// carrying an anchor tag with `numbered="false"`
/// (`## Copyright {% tag numbered="false" /%}`) — used for front-matter
/// sections (copyright, preface) that should precede `1.` without
/// consuming a number.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HeadingNumbering {
    /// Master switch. When `false` (the default) no numbering happens.
    pub enabled: bool,
    /// Deepest heading level that receives a number. `3` numbers
    /// `h1`/`h2`/`h3` and leaves `h4`+ plain. Clamped to `1..=6`.
    pub max_depth: u8,
    /// String placed between the last number and the heading text.
    /// Defaults to a single space (`"1.2 Title"`); set to e.g. `". "`
    /// for `"1.2. Title"`.
    pub separator: String,
}

impl Default for HeadingNumbering {
    fn default() -> Self {
        Self {
            enabled: false,
            max_depth: 3,
            separator: " ".to_string(),
        }
    }
}

/// How a callout is framed.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CalloutDecoration {
    /// Filled rectangle with optional border and a left accent stripe
    /// (the default — the "admonition box" look).
    #[default]
    Box,
    /// No fill or stripe — a horizontal rule above and below the content
    /// (the "bulletin / notice" look). The rule colour is the callout's
    /// `accent`; its thickness is the global `callout_rule_thickness`.
    Rules,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CalloutStyle {
    pub background: ColorRgb,
    pub border: ColorRgb,
    /// Left accent bar colour. Drawn as a thicker stripe on the left side
    /// of the box (`decoration = "box"`), or as the rule colour
    /// (`decoration = "rules"`).
    pub accent: ColorRgb,
    /// How the callout is framed. Defaults to a filled box.
    pub decoration: CalloutDecoration,
    /// Optional bold heading drawn as the first line of the box (e.g.
    /// `"WARNING"`). Empty / unset renders no label, preserving the
    /// plain-box behaviour. Typically uppercase.
    pub label: String,
    /// Colour for the `label`. Defaults to the document body text
    /// colour; set it (e.g. to the accent) to tint the heading.
    pub label_color: Option<ColorRgb>,
    /// Centre the label across the content column instead of
    /// left-aligning it. Used by the bulletin layout.
    pub label_centered: bool,
    /// Optional icon asset (any `AssetResolver` URI / path) drawn at the
    /// box's top-left, with the label and body indented past it. Empty /
    /// unset renders no icon.
    pub icon: String,
}

impl Default for CalloutStyle {
    fn default() -> Self {
        Self {
            background: ColorRgb::new(247, 248, 250),
            border: ColorRgb::new(220, 225, 230),
            accent: ColorRgb::new(120, 130, 145),
            decoration: CalloutDecoration::Box,
            label: String::new(),
            label_color: None,
            label_centered: false,
            icon: String::new(),
        }
    }
}

/// How `[text](url)` links are visually distinguished from body text.
/// The PDF link annotation is created regardless of these settings;
/// these only affect the glyphs underneath so readers spot the link
/// without hovering.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LinkStyle {
    /// Text colour applied to the link's glyphs.
    pub color: ColorRgb,
    /// Render link text in italic.
    pub italic: bool,
    /// Render link text in bold (font weight 700).
    pub bold: bool,
    /// Draw a stroked rule directly below each line of the link's
    /// text, in `color` and at `underline_thickness`. Useful when
    /// colour alone isn't enough to make the link stand out (e.g.
    /// for accessibility or B&W printing).
    pub underline: bool,
    /// Stroke thickness for the underline rule, in PDF points.
    /// Ignored when `underline = false`.
    pub underline_thickness: f32,
}

impl Default for LinkStyle {
    fn default() -> Self {
        Self {
            color: ColorRgb::new(20, 95, 175),
            italic: false,
            bold: false,
            underline: false,
            underline_thickness: 0.6,
        }
    }
}

/// Per-token-class colour palette for the built-in syntax highlighter.
/// Token classes a language doesn't produce simply go unused; languages
/// without a recognised name (or `language` attribute) fall back to
/// `code_text_color`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CodeHighlightStyle {
    pub keyword: ColorRgb,
    pub string: ColorRgb,
    pub comment: ColorRgb,
    pub number: ColorRgb,
}

impl Default for CodeHighlightStyle {
    fn default() -> Self {
        Self {
            keyword: ColorRgb::new(170, 50, 130),
            string: ColorRgb::new(70, 120, 50),
            comment: ColorRgb::new(120, 130, 140),
            number: ColorRgb::new(190, 110, 30),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CalloutStyles {
    pub note: CalloutStyle,
    pub info: CalloutStyle,
    pub warning: CalloutStyle,
    pub caution: CalloutStyle,
    pub danger: CalloutStyle,
    pub success: CalloutStyle,
    pub notice: CalloutStyle,
}

impl Default for CalloutStyles {
    fn default() -> Self {
        // Colours per kind; `label` / `label_color` / `icon` inherit the
        // label-less, icon-less defaults (`..CalloutStyle::default()`) so
        // existing themes render unchanged until they opt in.
        Self {
            note: CalloutStyle {
                background: ColorRgb::new(247, 248, 250),
                border: ColorRgb::new(220, 225, 230),
                accent: ColorRgb::new(120, 130, 145),
                ..CalloutStyle::default()
            },
            info: CalloutStyle {
                background: ColorRgb::new(232, 244, 253),
                border: ColorRgb::new(180, 213, 240),
                accent: ColorRgb::new(54, 130, 200),
                ..CalloutStyle::default()
            },
            warning: CalloutStyle {
                background: ColorRgb::new(255, 247, 230),
                border: ColorRgb::new(252, 211, 166),
                accent: ColorRgb::new(217, 119, 6),
                ..CalloutStyle::default()
            },
            caution: CalloutStyle {
                background: ColorRgb::new(255, 247, 230),
                border: ColorRgb::new(252, 211, 166),
                accent: ColorRgb::new(217, 119, 6),
                ..CalloutStyle::default()
            },
            danger: CalloutStyle {
                background: ColorRgb::new(254, 232, 232),
                border: ColorRgb::new(248, 187, 187),
                accent: ColorRgb::new(204, 51, 51),
                ..CalloutStyle::default()
            },
            success: CalloutStyle {
                background: ColorRgb::new(232, 250, 240),
                border: ColorRgb::new(168, 220, 188),
                accent: ColorRgb::new(46, 160, 100),
                ..CalloutStyle::default()
            },
            notice: CalloutStyle {
                background: ColorRgb::new(245, 240, 255),
                border: ColorRgb::new(214, 198, 240),
                accent: ColorRgb::new(120, 80, 200),
                ..CalloutStyle::default()
            },
        }
    }
}

impl CalloutStyles {
    pub fn for_kind(&self, kind: &str) -> &CalloutStyle {
        match kind {
            "info" => &self.info,
            "warning" => &self.warning,
            "caution" => &self.caution,
            "danger" => &self.danger,
            "success" => &self.success,
            "notice" => &self.notice,
            _ => &self.note,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PageLayoutStyle {
    /// Number of text columns on each body page (1 = default full-width column).
    pub columns: u8,
    /// Horizontal gap between columns, in points.
    pub gap: f32,
    /// Headings at this level and above span all columns (1 = H1 only).
    pub span_heading_through: u8,
}

impl Default for PageLayoutStyle {
    fn default() -> Self {
        Self {
            columns: 1,
            gap: 20.0,
            span_heading_through: 1,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Style {
    /// Schema-version stamp; reserved for future incompatible bumps.
    pub schema_version: u32,

    // ── Page geometry ─────────────────────────────────────────────────
    pub page_width: f32,
    pub page_height: f32,
    pub margin_x: f32,
    pub margin_y: f32,
    /// Multi-column body layout (1 = single column, the default).
    pub page_layout: PageLayoutStyle,
    /// When the rendered document ends on an odd page, append one page so
    /// the physical total is even. Intended for duplex (double-sided)
    /// printing, where each new document should begin on the front of a
    /// fresh sheet. The padding page carries the running header / footer
    /// (and watermark) like any other page but has no body content, and it
    /// is counted in the `{total}` page-of total. Off by default.
    pub pad_to_even: bool,

    // ── Body text ─────────────────────────────────────────────────────
    pub body_font_size: f32,
    pub body_line_height: f32,
    pub paragraph_space_after: f32,
    pub text_color: ColorRgb,
    /// Horizontal alignment for body prose (paragraphs, list-item and
    /// callout bodies). Headings, captions and table cells are
    /// unaffected. Defaults to `left`; `justify` spreads every line but
    /// the last to the column width.
    pub text_align: TextAlign,
    /// How `[text](url)` links are visually distinguished from body
    /// text. The PDF link annotation is created regardless; this is
    /// purely the visual cue so a reader spots the link before they
    /// hover over it.
    pub link: LinkStyle,

    // ── Headings ──────────────────────────────────────────────────────
    pub heading: HeadingStyles,
    /// Automatic `1` / `1.1` / `1.1.1` section numbering. Off by default.
    pub heading_numbering: HeadingNumbering,

    // ── Lists ─────────────────────────────────────────────────────────
    pub list_indent: f32,
    pub list_item_space_after: f32,
    pub list_marker_gap: f32,
    pub list_marker: ListMarkerStyle,

    // ── Block quotes ──────────────────────────────────────────────────
    pub blockquote_indent: f32,
    pub blockquote_bar_width: f32,
    pub blockquote_bar_color: ColorRgb,
    pub blockquote_text_color: ColorRgb,

    // ── Code ──────────────────────────────────────────────────────────
    pub code_font_family: String,
    pub code_font_size: f32,
    pub code_padding: f32,
    pub code_background: ColorRgb,
    pub code_text_color: ColorRgb,
    /// Per-token-class palette for the small built-in syntax
    /// highlighter. Applies to fenced code blocks whose `language`
    /// attribute is recognised.
    pub code_highlight: CodeHighlightStyle,

    // ── Callouts ──────────────────────────────────────────────────────
    pub callout_padding: f32,
    pub callout_accent_width: f32,
    pub callout_styles: CalloutStyles,
    pub callout_space_after: f32,
    /// Font size (pt) for a callout's bold label line.
    pub callout_label_size: f32,
    /// Square draw size (pt) for a callout's icon.
    pub callout_icon_size: f32,
    /// Horizontal gap (pt) between the icon and the label / body column.
    pub callout_icon_gap: f32,
    /// Stroke thickness (pt) for the rules of a `decoration = "rules"`
    /// callout.
    pub callout_rule_thickness: f32,

    // ── Horizontal rule ───────────────────────────────────────────────
    pub rule_color: ColorRgb,
    pub rule_thickness: f32,
    pub rule_space_around: f32,

    // ── Table of contents / list of figures / list of tables ─────────
    pub toc: TocStyle,
    pub lof: ListSectionStyle,
    pub lot: ListSectionStyle,

    // ── PDF export profile ────────────────────────────────────────────
    pub pdf_export: PdfExportProfile,

    // ── Page decoration (header / footer) ─────────────────────────────
    pub page_decoration: PageDecorationStyle,

    // ── Captions ──────────────────────────────────────────────────────
    pub caption_position: CaptionPosition,
    /// Colour for figure/table caption text. `None` falls back to
    /// `blockquote_text_color` (the historical default); a per-caption
    /// `{% caption color="#…" %}` overrides both.
    pub caption_color: Option<ColorRgb>,
    /// Prefix used for figure caption labels — `"<prefix> N"` or
    /// `"<prefix> N: caption"`. Defaults to "Figure"; common alternates
    /// include "Fig.", "Image", localised forms, etc.
    pub figure_caption_prefix: String,
    /// Prefix for table caption labels. Defaults to "Table"; common
    /// alternates include "Tab.".
    pub table_caption_prefix: String,
    /// Separator between the numbered prefix and the caption text
    /// (when present). Defaults to ":".
    pub caption_separator: String,
    /// When true, an image's `title` attribute (from `{% media title="…" /%}`
    /// or markdown `![alt](url "title")`) is used as the caption /
    /// accessibility text when neither an explicit `{% caption %}` nor `alt`
    /// is present. Off by default — `title` is otherwise ignored in the PDF,
    /// since it has no natural print rendering.
    pub image_title_fallback: bool,

    // ── Tables ────────────────────────────────────────────────────────
    pub table_column_sizing: TableColumnSizing,
    /// Explicit relative column widths. When set and the count matches a
    /// table's column count, the table's inner width is split in these
    /// proportions (weights, not absolute units — `[1.0, 3.0]` makes the
    /// second column three times the first) and `table_column_sizing` is
    /// ignored for that table. Ideal for key/value metadata tables where a
    /// fixed-width label column reads better than content-based sizing.
    /// Empty (the default) leaves every table on automatic sizing.
    pub table_column_weights: Vec<f32>,
    /// Which rules the table draws: a full `grid` (default), only
    /// `horizontal` row separators, or `none`.
    pub table_borders: TableBorders,
    pub table_cell_padding: f32,
    pub table_border_color: ColorRgb,
    pub table_border_thickness: f32,
    /// Colour of the table's outer frame — the top/bottom rules (and the
    /// left/right rules in `grid` mode). Unset means edges match the
    /// internal `table_border_color`; a darker edge gives a booktabs look.
    pub table_edge_color: Option<ColorRgb>,
    /// Thickness of those outer rules; defaults to `table_border_thickness`.
    pub table_edge_thickness: Option<f32>,
    pub table_header_background: ColorRgb,
    pub table_header_text_color: ColorRgb,
    /// Background fill for alternating body rows (zebra striping); applied
    /// to every other body row. `None` (the default) disables it. A table
    /// can override it with the `{% table stripe="#rrggbb" %}` attribute,
    /// or `stripe="none"` to switch striping off for that one table.
    pub table_stripe_color: Option<ColorRgb>,
    /// Treat the first column as a header column (row headers): its cells
    /// render in the header text style (bold, `table_header_text_color`)
    /// over a `table_header_background` fill, and are tagged as row headers
    /// for accessibility. Off by default; set per table with
    /// `{% table header_column=true %}`. Independent of the header row, so
    /// a table can have both.
    pub table_header_column: bool,
    pub table_space_after: f32,

    // ── Footnotes ─────────────────────────────────────────────────────
    pub footnote: FootnoteStyle,

    // ── Watermark / page background ───────────────────────────────────
    /// Optional watermark drawn beneath every page's body content. Use
    /// for letterhead images, "DRAFT" diagonals, confidentiality
    /// markings, etc. Tagged as `Artifact` so screen readers ignore it.
    pub watermark: Option<Watermark>,

    // ── Custom fonts ──────────────────────────────────────────────────
    /// File paths to additional `.ttf`/`.otf` fonts that should be
    /// loaded into the font collection before laying out the document.
    /// Any family they expose can then be referenced via `font_families`
    /// just like the bundled Noto family. Loaded once at the start of
    /// the render — paths are not re-checked.
    pub font_paths: Vec<String>,
    /// Family names used for body text (in fallback order). Empty list
    /// means "use the bundled defaults" (Noto Sans + the multi-script
    /// Noto fallbacks). Pass family names registered by your own
    /// `font_paths` to swap in a custom typeface.
    pub body_font_families: Vec<String>,

    // ── Hyphenation ───────────────────────────────────────────────────
    pub hyphenation: HyphenationStyle,

    // ── Cover page ────────────────────────────────────────────────────
    /// Synthesised cover page rendered before any body content. Drawn
    /// from frontmatter (title, description, authors, date) plus an
    /// optional logo. Source documents stay output-agnostic — all
    /// cover-page knobs live here in the style.
    pub coverpage: CoverPageStyle,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            schema_version: 1,
            page_width: 595.0,
            page_height: 842.0, // A4 — real page size now that we paginate
            margin_x: 72.0,
            margin_y: 72.0,
            page_layout: PageLayoutStyle::default(),
            pad_to_even: false,
            text_align: TextAlign::default(),
            body_font_size: 11.0,
            body_line_height: 1.5,
            paragraph_space_after: 8.0,
            text_color: ColorRgb::new(20, 20, 20),
            link: LinkStyle::default(),
            heading: HeadingStyles::default(),
            heading_numbering: HeadingNumbering::default(),
            list_indent: 24.0,
            list_item_space_after: 4.0,
            list_marker_gap: 8.0,
            list_marker: ListMarkerStyle::default(),
            blockquote_indent: 24.0,
            blockquote_bar_width: 3.0,
            blockquote_bar_color: ColorRgb::new(200, 205, 215),
            blockquote_text_color: ColorRgb::new(80, 90, 100),
            code_font_family: "Noto Sans Mono".into(),
            code_font_size: 10.0,
            code_padding: 12.0,
            code_background: ColorRgb::new(245, 246, 248),
            code_text_color: ColorRgb::new(40, 50, 70),
            code_highlight: CodeHighlightStyle::default(),
            callout_padding: 12.0,
            callout_accent_width: 4.0,
            callout_styles: CalloutStyles::default(),
            callout_space_after: 12.0,
            callout_label_size: 11.0,
            callout_icon_size: 20.0,
            callout_icon_gap: 10.0,
            callout_rule_thickness: 0.7,
            rule_color: ColorRgb::new(200, 205, 215),
            rule_thickness: 0.75,
            rule_space_around: 12.0,
            toc: TocStyle::default(),
            lof: ListSectionStyle::default(),
            lot: ListSectionStyle::default(),
            pdf_export: PdfExportProfile::default(),
            page_decoration: PageDecorationStyle::default(),
            caption_position: CaptionPosition::Above,
            caption_color: None,
            figure_caption_prefix: "Figure".to_string(),
            table_caption_prefix: "Table".to_string(),
            caption_separator: ":".to_string(),
            image_title_fallback: false,
            table_column_sizing: TableColumnSizing::Auto,
            table_column_weights: Vec::new(),
            table_borders: TableBorders::Grid,
            table_cell_padding: 6.0,
            table_border_color: ColorRgb::new(210, 215, 225),
            table_border_thickness: 0.5,
            table_edge_color: None,
            table_edge_thickness: None,
            table_header_background: ColorRgb::new(240, 242, 246),
            table_header_text_color: ColorRgb::new(20, 30, 50),
            table_stripe_color: None,
            table_header_column: false,
            table_space_after: 12.0,
            footnote: FootnoteStyle::default(),
            watermark: None,
            font_paths: Vec::new(),
            body_font_families: Vec::new(),
            hyphenation: HyphenationStyle::default(),
            coverpage: CoverPageStyle::default(),
        }
    }
}

/// A page-level watermark. Use the `kind` field to pick image vs text.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Watermark {
    pub kind: WatermarkKind,
    /// 0.0 (fully transparent) to 1.0 (fully opaque). Defaults to 0.15
    /// which gives a subtle "DRAFT"-style overlay.
    pub opacity: f32,
    /// If true, the first page renders without the watermark — useful
    /// when the cover/title page should stay clean.
    pub skip_first_page: bool,
}

impl Default for Watermark {
    fn default() -> Self {
        Self {
            kind: WatermarkKind::Text(WatermarkText::default()),
            opacity: 0.15,
            skip_first_page: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WatermarkKind {
    /// A static image stretched to the configured size and anchored
    /// to a page-relative position (top-left origin, in PDF points).
    Image(WatermarkImage),
    /// Diagonal text overlay across the page centre. Rotation in
    /// degrees, anti-clockwise; e.g. `-30.0` slants from bottom-left
    /// to top-right.
    Text(WatermarkText),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WatermarkImage {
    pub src: String,
    /// Top-left origin in PDF points. Defaults to (0, 0).
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WatermarkText {
    pub text: String,
    pub font_size: f32,
    pub color: ColorRgb,
    pub rotation_deg: f32,
}

impl Default for WatermarkText {
    fn default() -> Self {
        Self {
            text: "DRAFT".to_string(),
            font_size: 96.0,
            color: ColorRgb::new(180, 180, 180),
            rotation_deg: -30.0,
        }
    }
}

/// Synthesised cover/title page placed before any body content. The
/// renderer pulls title / description / authors / date from
/// `RenderContext` (which the CLI populates from frontmatter), plus an
/// optional logo image. Disabled by default — most documents don't
/// need a cover page.
///
/// Layout is centred vertically by the `top_margin` you pick; entries
/// stack with explicit gaps between them. Set `subtitle` to a template
/// string (`{description}`, `{date}`, etc.) when you want anything
/// below the title.
/// Where the optional logo / hero image sits in the cover-page stack.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LogoPosition {
    /// At the top of the page, above the title. The default — fits
    /// the "letterhead + report title" pattern.
    #[default]
    Above,
    /// Between the title and the subtitle. Use this when the front
    /// page leads with a big title and the image acts as a hero
    /// rather than a small mark.
    BelowTitle,
}

/// Horizontal alignment of the cover-page text blocks.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CoverAlign {
    /// Centred in the body column (the default).
    #[default]
    Center,
    /// Flush against the left margin — fits a "title page" look where
    /// the title and metadata stack at the top-left.
    Left,
}

/// Horizontal alignment for body prose. Mirrors CSS `text-align`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    /// Flush against the start (left for LTR) edge — the default.
    #[default]
    Left,
    /// Spread every line but the last to fill the column width.
    Justify,
    /// Centre each line within the column.
    Center,
    /// Flush against the end (right for LTR) edge.
    Right,
}

impl TextAlign {
    /// The parley alignment this maps to.
    pub fn to_parley(self) -> parley::layout::Alignment {
        use parley::layout::Alignment;
        match self {
            TextAlign::Left => Alignment::Start,
            TextAlign::Justify => Alignment::Justify,
            TextAlign::Center => Alignment::Center,
            TextAlign::Right => Alignment::End,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CoverPageStyle {
    pub enabled: bool,
    /// Horizontal page margin for the cover page only, in points. Defaults
    /// to the document `margin_x`. A smaller value lets cover art (the hero
    /// image) bleed wider than the body text column.
    pub margin_x: Option<f32>,
    /// Vertical page margin for the cover page only, in points. Defaults to
    /// the document `margin_y`. A smaller value gives the cover more room
    /// (e.g. for a tall hero) and lifts the logo toward the page top. Only
    /// honoured when the cover page skips its header / footer.
    pub margin_y: Option<f32>,
    pub logo: Option<LogoSpec>,
    /// Where the logo sits relative to the title. See [`LogoPosition`].
    pub logo_position: LogoPosition,
    /// Optional hero image (e.g. a product photo) drawn below the cover
    /// metadata — a second image slot so a cover can show both a brand
    /// logo (above the title) and a hero image.
    pub hero: Option<LogoSpec>,
    /// Gap above the hero image.
    pub hero_gap: f32,
    /// Vertical space above the first element on the page (the logo
    /// when `logo_position = "above"`, otherwise the title).
    /// Defaults to ~1/4 of A4 height.
    pub top_margin: f32,
    /// Gap between the logo image and the title text — applied
    /// regardless of `logo_position`. When `above`, the gap is below
    /// the logo; when `below_title`, it's between the title and the
    /// logo.
    pub logo_to_title_gap: f32,
    pub title_font_size: f32,
    /// Optional accent run appended inline to the title and rendered in
    /// a lighter weight and (optionally) a different colour — e.g. a
    /// title of "MyProduct" with `title_accent = " Manual"` renders the
    /// product name bold and "Manual" in a muted accent beside it. The
    /// string is a template (`{title}` / `{date}` / any frontmatter var)
    /// so the accent can come from metadata; empty omits it.
    pub title_accent: String,
    /// Colour for `title_accent`. Defaults to `text_color` when unset.
    pub title_accent_color: Option<ColorRgb>,
    pub title_to_subtitle_gap: f32,
    /// Template string for the subtitle — supports the same
    /// `{title}`/`{description}`/`{date}` substitutions as headers.
    /// Empty string omits the subtitle entirely.
    pub subtitle: String,
    pub subtitle_font_size: f32,
    pub subtitle_to_authors_gap: f32,
    pub show_authors: bool,
    pub authors_font_size: f32,
    pub authors_to_date_gap: f32,
    pub show_date: bool,
    pub date_font_size: f32,
    pub text_color: ColorRgb,
    /// Horizontal alignment of every cover text block (title, subtitle,
    /// detail lines, authors, date). Defaults to centred.
    pub align: CoverAlign,
    /// Extra metadata lines rendered under the title, each a template
    /// string (`{title}` / `{description}` / `{date}` / any
    /// `RenderContext` var). Use for "Date: {date}", "Version:
    /// {version}", etc. A line whose substitution is empty is skipped.
    pub detail_lines: Vec<String>,
    /// Font size for `detail_lines`.
    pub detail_font_size: f32,
    /// Gap below the title before the detail lines.
    pub title_to_detail_gap: f32,
    /// Vertical gap between consecutive detail lines.
    pub detail_line_gap: f32,
    /// Colour for `detail_lines`. Defaults to `text_color` when unset;
    /// typically a muted grey.
    pub detail_color: Option<ColorRgb>,
    /// When `true`, insert a fully blank page directly after the
    /// cover page so the body content starts on page 3 — a recto in
    /// double-sided printing. Useful for printed reports / books;
    /// no effect when the document is read on screen.
    pub blank_page_after: bool,
}

impl Default for CoverPageStyle {
    fn default() -> Self {
        Self {
            enabled: false,
            margin_x: None,
            margin_y: None,
            logo: None,
            logo_position: LogoPosition::Above,
            hero: None,
            hero_gap: 40.0,
            top_margin: 200.0,
            logo_to_title_gap: 32.0,
            title_font_size: 32.0,
            title_accent: String::new(),
            title_accent_color: None,
            title_to_subtitle_gap: 12.0,
            subtitle: String::new(),
            subtitle_font_size: 14.0,
            subtitle_to_authors_gap: 24.0,
            show_authors: true,
            authors_font_size: 12.0,
            authors_to_date_gap: 8.0,
            show_date: true,
            date_font_size: 11.0,
            text_color: ColorRgb::new(20, 20, 20),
            align: CoverAlign::Center,
            detail_lines: Vec::new(),
            detail_font_size: 11.0,
            title_to_detail_gap: 14.0,
            detail_line_gap: 3.0,
            detail_color: None,
            blank_page_after: false,
        }
    }
}

/// Ordered-list numbering style for one nesting level. Mirrors the CSS
/// `list-style-type` values of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MarkerSequence {
    /// 1, 2, 3, …
    Decimal,
    /// a, b, c, … z, aa, ab, …
    LowerAlpha,
    /// A, B, C, …
    UpperAlpha,
    /// i, ii, iii, iv, …
    LowerRoman,
    /// I, II, III, IV, …
    UpperRoman,
}

/// Styling for list-item markers. Bullets (unordered lists) are
/// unaffected; these knobs shape ordered-list numbering and the
/// optional circular badge treatment.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ListMarkerStyle {
    /// Ordered-list numbering style per nesting depth — index 0 is the
    /// outermost list, deeper levels cycle through the list (wrapping
    /// when the nesting runs deeper than the list). Empty means decimal
    /// at every depth, e.g. `["decimal", "lower-alpha", "lower-roman"]`
    /// gives `1.` → `a.` → `i.` as lists nest.
    pub ordered_sequences: Vec<MarkerSequence>,
    /// Draw each ordered marker centred inside a filled circle. The
    /// trailing `.` is dropped — the badge itself delimits the marker.
    pub badge: bool,
    /// Badge fill colour.
    pub badge_fill: ColorRgb,
    /// Marker text colour inside a badge. Defaults to the body text
    /// colour when unset.
    pub badge_text_color: Option<ColorRgb>,
    /// Badge diameter as a multiple of the marker font size.
    pub badge_scale: f32,
}

impl Default for ListMarkerStyle {
    fn default() -> Self {
        Self {
            ordered_sequences: Vec::new(),
            badge: false,
            badge_fill: ColorRgb::new(223, 227, 232),
            badge_text_color: None,
            badge_scale: 1.7,
        }
    }
}

impl ListMarkerStyle {
    /// The numbering style to use at the given nesting depth (0 = the
    /// outermost ordered list).
    pub fn sequence_for_depth(&self, depth: usize) -> MarkerSequence {
        if self.ordered_sequences.is_empty() {
            MarkerSequence::Decimal
        } else {
            self.ordered_sequences[depth % self.ordered_sequences.len()]
        }
    }
}

/// Word-hyphenation settings. When `enabled`, the renderer pre-walks
/// body text and inserts soft hyphens (U+00AD) at every Knuth–Liang
/// hyphenation point for the configured language. parley breaks lines
/// at those points when needed and the hyphens stay invisible
/// otherwise.
///
/// Only English-US is bundled by default — other languages need a
/// `.bincode` pattern file built with the `hyphenation` crate's
/// `build_dictionaries` feature, supplied via `dictionary_path`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HyphenationStyle {
    pub enabled: bool,
    /// Language tag (e.g. `"en-us"`). The bundled tag is `"en-us"`;
    /// other languages need `dictionary_path`.
    pub language: String,
    /// Don't hyphenate words shorter than this. Defaults to 5.
    pub min_word_chars: u8,
    /// Optional path to a `.bincode` hyphenation pattern file (built
    /// from the upstream `hyphenation` repo). Required when
    /// `language` isn't the bundled `"en-us"`.
    pub dictionary_path: Option<String>,
}

impl Default for HyphenationStyle {
    fn default() -> Self {
        Self {
            enabled: false,
            language: "en-us".to_string(),
            min_word_chars: 5,
            dictionary_path: None,
        }
    }
}

/// Per-page footnote pool styling.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FootnoteStyle {
    /// Body font size for footnote entries (typically smaller than
    /// body text — e.g. 9pt against an 11pt body).
    pub font_size: f32,
    /// Line height (em multiplier) for footnote entries.
    pub line_height: f32,
    /// Vertical gap between consecutive footnote entries on the page.
    pub entry_space_after: f32,
    /// Vertical gap between body content and the separator rule.
    pub gap_above: f32,
    /// Vertical gap between the separator rule and the first entry.
    pub gap_below_rule: f32,
    /// Width of the separator rule, expressed as a fraction of the
    /// printable column width (0.0–1.0). Use 1.0 for a full-width
    /// rule, 0.3 for a short academic-style separator.
    pub rule_width_frac: f32,
    /// Stroke thickness for the separator rule.
    pub rule_thickness: f32,
    pub rule_color: ColorRgb,
    pub text_color: ColorRgb,
}

impl Default for FootnoteStyle {
    fn default() -> Self {
        Self {
            font_size: 9.0,
            line_height: 1.35,
            entry_space_after: 3.0,
            gap_above: 12.0,
            gap_below_rule: 6.0,
            rule_width_frac: 0.3,
            rule_thickness: 0.5,
            rule_color: ColorRgb::new(150, 155, 165),
            text_color: ColorRgb::new(70, 80, 95),
        }
    }
}

/// Table of contents — generated from the document's heading outline.
/// Disabled by default. When enabled, the TOC pages are inserted at
/// `position` and each entry links to its target heading via PDF
/// internal destinations (powered by the same anchor mechanism that
/// `{% tagref %}` uses, with synthetic anchor ids assigned to every
/// heading).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TocStyle {
    pub enabled: bool,
    pub position: TocPosition,
    /// Title to render at the top of the TOC (e.g. "Table of Contents").
    pub title: String,
    /// Font size for the TOC's title heading.
    pub title_font_size: f32,
    /// Font size for individual entries.
    pub entry_font_size: f32,
    /// Vertical gap after each entry.
    pub entry_space_after: f32,
    /// Horizontal indent applied per heading-level depth.
    pub entry_indent_per_level: f32,
    /// Maximum heading level included (1 = h1 only, 6 = all).
    pub max_depth: u8,
}

impl Default for TocStyle {
    fn default() -> Self {
        Self {
            enabled: false,
            position: TocPosition::Start,
            title: "Table of Contents".to_string(),
            title_font_size: 24.0,
            entry_font_size: 11.0,
            entry_space_after: 4.0,
            entry_indent_per_level: 16.0,
            max_depth: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TocPosition {
    #[default]
    Start,
    End,
}

/// Where the visual caption block sits relative to its figure or table.
/// Applies to both `{% caption %}`-attached and alt-derived captions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptionPosition {
    #[default]
    Above,
    Below,
}

/// Strategy for sizing table columns.
///
/// - `Auto`: measure each cell's natural width and longest-word width,
///   distribute the available column space proportionally between them.
///   Compact tables stay narrow; wide tables fill the column.
/// - `Equal`: every column gets `available_width / num_cols` (the
///   pre-auto behaviour). Predictable but wastes space on tables with
///   short columns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableColumnSizing {
    #[default]
    Auto,
    Equal,
}

/// Which rules a table draws.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableBorders {
    /// Full grid — outer box plus every row and column separator (the
    /// default).
    #[default]
    Grid,
    /// Horizontal row separators only; no verticals or outer box. The
    /// clean "ruled" look for key-value / metadata tables.
    Horizontal,
    /// No rules at all — cells are separated by whitespace (and the
    /// optional header fill) only.
    None,
}

/// Generic auto-generated list section: List of Figures, List of
/// Tables. Same shape as a TOC but flat (no level indenting). The
/// `title` field is optional so callers can omit it from TOML and let
/// the renderer pick a sensible default per section ("List of Figures"
/// for `lof`, "List of Tables" for `lot`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ListSectionStyle {
    pub enabled: bool,
    pub position: TocPosition,
    pub title: Option<String>,
    #[serde(default = "default_list_title_size")]
    pub title_font_size: f32,
    #[serde(default = "default_list_entry_size")]
    pub entry_font_size: f32,
    #[serde(default = "default_list_entry_space_after")]
    pub entry_space_after: f32,
}

fn default_list_title_size() -> f32 {
    24.0
}
fn default_list_entry_size() -> f32 {
    11.0
}
fn default_list_entry_space_after() -> f32 {
    4.0
}

impl ListSectionStyle {
    /// Title to display, falling back to the supplied default when
    /// the caller didn't override it.
    pub fn resolved_title<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.title.as_deref().unwrap_or(fallback)
    }
}

/// PDF export profile — selects the krilla validator and provides
/// document-metadata defaults that the renderer applies via `RenderContext`.
///
/// PDF/A profiles require certain compliance items (embedded subset fonts,
/// ToUnicode CMaps, document metadata). krilla handles the technical
/// compliance automatically; the caller's only job is to provide title,
/// language, and creator strings via `RenderContext` (the bin pulls these
/// from the document's frontmatter).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfExportProfile {
    /// Plain PDF, no validator. Smallest file size; no compliance guarantees.
    #[default]
    None,
    /// PDF/A-1b — long-term archival, level B (basic visual reproduction).
    A1B,
    /// PDF/A-2b — extends A-1b with JPEG 2000, transparency, and more.
    A2B,
    /// PDF/A-3b — like A-2b but allows arbitrary embedded files.
    A3B,
    /// PDF/A-4 — the modern archival profile (PDF 2.0 base).
    A4,
    /// PDF/UA-1 — universal accessibility. Requires tagged PDF (deferred).
    UA1,
}

/// Page-level header/footer decoration. Both are optional — by default
/// no decoration is drawn, matching the previous behaviour.
///
/// Template strings support these variables:
///   - `{page}`    — current page number, 1-indexed
///   - `{total}`   — total page count
///   - `{title}`   — document title (set via `Style::with_title()` at runtime)
///   - `{chapter}` — most recent h1 heading text on or before this page
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PageDecorationStyle {
    pub header: Option<HeaderFooterStyle>,
    pub footer: Option<HeaderFooterStyle>,
    /// Optional rich "banner" drawn at the top of every page above the
    /// body — a taller masthead than the 3-slot header, for bulletin /
    /// notice documents. Reserves its `height` so the body starts below
    /// it. See [`NoticeBanner`].
    pub banner: Option<NoticeBanner>,
    /// If true, the first page is rendered without header/footer (useful
    /// when the first page is a title or cover page).
    pub skip_first_page: bool,
    /// Optional QR-code stamp on the last content page — the printed-guide
    /// pattern where production scans a small code (encoding the document
    /// number) when packing. Drawn in the bottom-right corner above its
    /// human-readable label. See [`LastPageQr`].
    pub last_page_qr: Option<LastPageQr>,
}

/// A small QR code stamped in a bottom corner of the last page (per `align`)
/// that carries content, above a human-readable label. `value` and `label` are
/// templates (default `"{documentNumber}"`, resolved against the document's
/// frontmatter and the usual `{page}`/`{title}`/… tokens); a `value` that
/// doesn't resolve (no such field) skips the stamp entirely, and an empty
/// resolved `label` hides the caption.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LastPageQr {
    /// Template for the data encoded in the code.
    pub value: String,
    /// Template for the caption drawn beneath the code (empty hides it).
    pub label: String,
    /// Rendered side length of the code, in points.
    pub size: f32,
    /// Error-correction level: `low` | `medium` | `quartile` | `high`.
    pub ecl: String,
    /// Horizontal placement on the page: `left` | `center` | `right`
    /// (default). `left`/`right` are inset by `margin_left` / `margin_right`;
    /// `center` ignores both and sits the code on the page centre-line.
    pub align: String,
    /// Gap from the page's left edge to the code's left edge, in points
    /// (used when `align = "left"`).
    pub margin_left: f32,
    /// Gap from the page's right edge to the code's right edge, in points
    /// (used when `align = "right"`, the default).
    pub margin_right: f32,
    /// Gap from the page's bottom edge to the caption's bottom, in points —
    /// set this larger than the footer band so the stamp clears it.
    pub margin_bottom: f32,
    /// Module (foreground) colour; the field stays white.
    pub color: ColorRgb,
    /// Font size of the caption.
    pub label_font_size: f32,
    /// When the document pads to an even page count, place the stamp on the
    /// physical last page — the blank duplex-padding page — instead of the
    /// last page carrying body content. Off by default (the stamp avoids the
    /// blank filler). No effect when there is no padding page.
    pub include_padding_page: bool,
}

impl Default for LastPageQr {
    fn default() -> Self {
        Self {
            value: "{documentNumber}".to_string(),
            label: "{documentNumber}".to_string(),
            size: 54.0,
            ecl: "medium".to_string(),
            align: "right".to_string(),
            margin_left: 40.0,
            margin_right: 40.0,
            margin_bottom: 48.0,
            color: ColorRgb::new(0, 0, 0),
            label_font_size: 8.0,
            include_padding_page: false,
        }
    }
}

/// A masthead drawn at the top of every page: a logo (with an optional
/// subtitle beneath it) on the left, a wrapping disclaimer paragraph
/// below them, an icon on the right whose label is centred beneath it
/// just above an optional note line + full-width rule closing the band.
///
/// All text fields are templates (`{title}` / `{date}` / `{language}` /
/// any `RenderContext` var). Every piece is optional; an empty field is
/// simply skipped. The band reserves `height` points at the top of the
/// page so body content never collides with it.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NoticeBanner {
    /// Reserved vertical space (pt) from the page top.
    pub height: f32,
    /// Left masthead logo.
    pub logo: Option<LogoSpec>,
    /// Small line drawn directly under the logo (e.g. a company name).
    pub logo_subtitle: String,
    pub logo_subtitle_color: ColorRgb,
    pub logo_subtitle_font_size: f32,
    /// Wrapping disclaimer paragraph, below the logo block.
    pub disclaimer: String,
    pub disclaimer_color: ColorRgb,
    pub disclaimer_font_size: f32,
    /// Max lines the disclaimer may wrap into (reserves no extra space;
    /// the band `height` governs layout).
    pub disclaimer_max_lines: u8,
    /// Right-side icon (e.g. a warning triangle).
    pub icon: Option<LogoSpec>,
    /// Label centred under the icon, just above the closing rule (e.g.
    /// `"Safety Notice"`).
    pub label: String,
    pub label_color: ColorRgb,
    pub label_font_size: f32,
    /// A note line near the band's bottom (e.g.
    /// `"Original language: {language}"`).
    pub note: String,
    pub note_color: ColorRgb,
    pub note_font_size: f32,
    /// Full-width rule closing the band (above the body).
    pub rule: bool,
    pub rule_color: ColorRgb,
    pub rule_thickness: f32,
}

impl Default for NoticeBanner {
    fn default() -> Self {
        Self {
            height: 100.0,
            logo: None,
            logo_subtitle: String::new(),
            logo_subtitle_color: ColorRgb::new(140, 145, 150),
            logo_subtitle_font_size: 6.0,
            disclaimer: String::new(),
            disclaimer_color: ColorRgb::new(150, 155, 160),
            disclaimer_font_size: 8.0,
            disclaimer_max_lines: 3,
            icon: None,
            label: String::new(),
            label_color: ColorRgb::new(20, 20, 20),
            label_font_size: 11.0,
            note: String::new(),
            note_color: ColorRgb::new(60, 60, 60),
            note_font_size: 8.5,
            rule: true,
            rule_color: ColorRgb::new(180, 185, 190),
            rule_thickness: 0.6,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HeaderFooterStyle {
    pub left: String,
    pub center: String,
    pub right: String,
    pub font_size: f32,
    pub color: ColorRgb,
    /// Distance from the page edge (top edge for header, bottom edge for footer).
    pub margin_from_edge: f32,
    /// If true, draws a thin separator rule between the decoration and
    /// the body (under a header, above a footer).
    pub rule: bool,
    pub rule_color: ColorRgb,
    pub rule_thickness: f32,
    /// Vertical gap between the text and the rule (when `rule = true`).
    pub rule_gap: f32,
    /// Maximum number of text lines a slot is allowed to wrap into.
    /// Pagination reserves vertical space for this many lines so the
    /// body never collides with a multi-line band. Defaults to 1.
    pub max_lines: u8,
    /// Optional logo image for each slot. When set, the logo replaces
    /// any text template for that slot. Sized exactly as configured —
    /// no automatic scaling; pick `width` and `height` to match your
    /// reserved band height.
    pub logo_left: Option<LogoSpec>,
    pub logo_center: Option<LogoSpec>,
    pub logo_right: Option<LogoSpec>,
    /// Optional even-page (verso) slot overrides. Page 1 is treated
    /// as recto (odd). When set and the current page is even, the
    /// corresponding slot template is used in place of the odd
    /// counterpart — typical book layout has e.g. chapter title on
    /// recto, document title on verso. Empty strings mean "fall back
    /// to the odd slot".
    pub even: Option<HeaderFooterSlots>,
    /// Per-h1-chapter slot overrides. Keyed by exact h1 text; the
    /// renderer switches as soon as a heading at that level is seen
    /// on the page, so the same page never mixes two chapters'
    /// headers. Empty string slots fall back to the parent.
    pub per_chapter: HashMap<String, HeaderFooterSlots>,
}

/// Just the three slot templates — used for even-page variants and
/// per-chapter overrides. Empty fields fall back to the parent
/// `HeaderFooterStyle`'s slot.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HeaderFooterSlots {
    pub left: String,
    pub center: String,
    pub right: String,
}

/// A header/footer logo: an asset URI plus the rendered display size
/// in PDF points. The renderer decodes the asset once per document
/// and caches the result by URI, so repeating the same logo across
/// pages costs the same as decoding it for one page.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LogoSpec {
    /// Asset URI — anything the configured asset resolver understands
    /// (`file://path`, relative path, `https://…`, `arca://…`).
    pub src: String,
    pub width: f32,
    pub height: f32,
    /// Gap between the logo and the slot's text when both are present.
    /// Only effective for the LEFT and RIGHT slots — CENTER slot
    /// remains logo-or-text. Defaults to 6 pt.
    pub gap: f32,
}

impl Default for LogoSpec {
    fn default() -> Self {
        Self {
            src: String::new(),
            width: 0.0,
            height: 0.0,
            gap: 6.0,
        }
    }
}

impl Default for HeaderFooterStyle {
    fn default() -> Self {
        Self {
            left: String::new(),
            center: String::new(),
            right: String::new(),
            font_size: 9.0,
            color: ColorRgb::new(110, 120, 130),
            margin_from_edge: 36.0,
            rule: false,
            rule_color: ColorRgb::new(220, 225, 230),
            rule_thickness: 0.5,
            rule_gap: 4.0,
            max_lines: 1,
            logo_left: None,
            logo_center: None,
            logo_right: None,
            even: None,
            per_chapter: HashMap::new(),
        }
    }
}

impl HeaderFooterStyle {
    /// Resolve the `(left, center, right)` slot strings effective for
    /// this page given its parity and current chapter. Per-chapter
    /// overrides take precedence; even-page overrides apply next;
    /// missing/empty fields fall back to the parent slot.
    pub fn resolved_slots(&self, page_number: usize, chapter: &str) -> (String, String, String) {
        let mut left = self.left.clone();
        let mut center = self.center.clone();
        let mut right = self.right.clone();
        if page_number.is_multiple_of(2)
            && let Some(e) = &self.even
        {
            if !e.left.is_empty() {
                left = e.left.clone();
            }
            if !e.center.is_empty() {
                center = e.center.clone();
            }
            if !e.right.is_empty() {
                right = e.right.clone();
            }
        }
        if !chapter.is_empty()
            && let Some(c) = self.per_chapter.get(chapter)
        {
            if !c.left.is_empty() {
                left = c.left.clone();
            }
            if !c.center.is_empty() {
                center = c.center.clone();
            }
            if !c.right.is_empty() {
                right = c.right.clone();
            }
        }
        (left, center, right)
    }
}

impl HeaderFooterStyle {
    /// Total vertical space this decoration consumes from one edge of
    /// the page (text height × max_lines + rule gap + rule thickness).
    pub fn reserved_height(&self) -> f32 {
        let lines = self.max_lines.max(1) as f32;
        let text = self.font_size * 1.2 * lines;
        let rule = if self.rule {
            self.rule_gap + self.rule_thickness
        } else {
            0.0
        };
        text + rule
    }
}

impl Style {
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn from_toml_file(path: impl AsRef<std::path::Path>) -> Result<Self, StyleLoadError> {
        let text = std::fs::read_to_string(path).map_err(StyleLoadError::Io)?;
        Self::from_toml_str(&text).map_err(StyleLoadError::Toml)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StyleLoadError {
    #[error("failed to read style file: {0}")]
    Io(std::io::Error),
    #[error("failed to parse style TOML: {0}")]
    Toml(toml::de::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_loads() {
        let s = Style::default();
        assert_eq!(s.page_width, 595.0);
        assert_eq!(s.heading.h1.font_size, 26.0);
    }

    #[test]
    fn partial_toml_overrides_only_specified_fields() {
        let toml = r#"
page_width = 612.0
page_height = 792.0
margin_x = 54.0
"#;
        let s = Style::from_toml_str(toml).unwrap();
        // Letter-paper overrides applied.
        assert_eq!(s.page_width, 612.0);
        assert_eq!(s.page_height, 792.0);
        assert_eq!(s.margin_x, 54.0);
        // Untouched fields keep defaults.
        assert_eq!(s.body_font_size, 11.0);
        assert_eq!(s.heading.h1.font_size, 26.0);
        assert_eq!(
            s.callout_styles.warning.background,
            ColorRgb::new(255, 247, 230)
        );
    }

    #[test]
    fn nested_overrides_work() {
        let toml = r#"
[heading.h1]
font_size = 32.0
"#;
        let s = Style::from_toml_str(toml).unwrap();
        assert_eq!(s.heading.h1.font_size, 32.0);
        // Other heading levels keep defaults.
        assert_eq!(s.heading.h2.font_size, 21.0);
    }

    #[test]
    fn table_borders_default_and_override() {
        // Defaults to a full grid.
        assert_eq!(Style::default().table_borders, TableBorders::Grid);
        // snake_case values parse from a top-level key.
        let horizontal = Style::from_toml_str("table_borders = \"horizontal\"").unwrap();
        assert_eq!(horizontal.table_borders, TableBorders::Horizontal);
        let none = Style::from_toml_str("table_borders = \"none\"").unwrap();
        assert_eq!(none.table_borders, TableBorders::None);
    }

    #[test]
    fn text_align_defaults_left_and_parses() {
        assert_eq!(Style::default().text_align, TextAlign::Left);
        assert_eq!(
            Style::from_toml_str("text_align = \"justify\"")
                .unwrap()
                .text_align,
            TextAlign::Justify
        );
        assert_eq!(
            TextAlign::Justify.to_parley(),
            parley::layout::Alignment::Justify
        );
    }

    #[test]
    fn table_edge_color_optional() {
        // Unset by default — edges match the internal border colour.
        let d = Style::default();
        assert!(d.table_edge_color.is_none());
        assert!(d.table_edge_thickness.is_none());
        // A darker, thicker edge (booktabs look) parses.
        let s = Style::from_toml_str("table_edge_color = [0, 0, 0]\ntable_edge_thickness = 1.0")
            .unwrap();
        assert_eq!(s.table_edge_color, Some(ColorRgb::new(0, 0, 0)));
        assert_eq!(s.table_edge_thickness, Some(1.0));
    }

    #[test]
    fn list_marker_badge_and_sequences() {
        // Defaults: no badge, decimal at every depth.
        let d = Style::default().list_marker;
        assert!(!d.badge);
        assert_eq!(d.sequence_for_depth(0), MarkerSequence::Decimal);
        assert_eq!(d.sequence_for_depth(3), MarkerSequence::Decimal);

        let toml = r#"
[list_marker]
badge = true
ordered_sequences = ["decimal", "lower-alpha", "lower-roman"]
badge_fill = [223, 227, 232]
"#;
        let lm = Style::from_toml_str(toml).unwrap().list_marker;
        assert!(lm.badge);
        assert_eq!(lm.badge_fill, ColorRgb::new(223, 227, 232));
        // Depth cycles through the configured sequence and wraps.
        assert_eq!(lm.sequence_for_depth(0), MarkerSequence::Decimal);
        assert_eq!(lm.sequence_for_depth(1), MarkerSequence::LowerAlpha);
        assert_eq!(lm.sequence_for_depth(2), MarkerSequence::LowerRoman);
        assert_eq!(lm.sequence_for_depth(3), MarkerSequence::Decimal);
    }

    #[test]
    fn callout_kind_lookup() {
        let s = Style::default();
        assert_eq!(
            s.callout_styles.for_kind("warning").background,
            ColorRgb::new(255, 247, 230)
        );
        assert_eq!(
            s.callout_styles.for_kind("info").background,
            ColorRgb::new(232, 244, 253)
        );
        // Unknown kind falls back to note.
        assert_eq!(
            s.callout_styles.for_kind("unknown").background,
            s.callout_styles.note.background
        );
    }
}
