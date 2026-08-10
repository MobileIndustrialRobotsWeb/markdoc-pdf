//! parley → krilla bridge: build a styled `Layout` and emit its glyphs
//! to a krilla surface at a given origin.

use std::borrow::Cow;
use std::collections::HashMap;

use krilla::color::rgb;
use krilla::geom::Point;
use krilla::num::NormalizedF32;
use krilla::paint::Fill;
use krilla::tagging::{ContentTag, Identifier, SpanTag};
use krilla::text::{Font, GlyphId, KrillaGlyph};
use parley::layout::{Alignment, YieldData};
use parley::style::{
    FontFamily, FontFamilyName, FontStyle, FontWeight, LineHeight, OverflowWrap, StyleProperty,
};
use parley::{FontContext, InlineBox, InlineBoxKind, Layout, LayoutContext};

use super::inline::{InlineProp, InlineRange, LinkRange};

/// Parameters for laying out a styled text block.
pub struct TextStyle<'a> {
    pub font_size: f32,
    pub font_weight: f32,
    pub line_height: f32,
    pub color: rgb::Color,
    pub font_families: &'a [&'static str],
    /// When true, the entire layout is rendered in italic.
    pub italic: bool,
}

/// Lay out `text` at single-line (huge max_advance) and return the
/// rendered width of its first line in PDF points. Used by the TOC
/// builder to compute leader-dot fills.
pub fn measure_first_line_width(
    text: &str,
    style: &TextStyle<'_>,
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<rgb::Color>,
) -> f32 {
    let layout = build_layout(text, &[], style, 1_000_000.0, font_cx, layout_cx);
    layout
        .lines()
        .next()
        .map(|l| l.metrics().advance)
        .unwrap_or(0.0)
}

pub fn default_families() -> &'static [&'static str] {
    // parley walks this list per codepoint and uses the first family
    // whose glyph table covers it. Order is by likelihood / coverage:
    // primary text first, then script-specific Noto families, then
    // symbol/math fallbacks, then broad cross-platform Unicode fonts
    // for anything else (arrows, mathematical operators, …), and
    // emoji at the tail.
    &[
        // Primary Latin / Greek / Cyrillic face.
        "Noto Sans",
        // Script-specific families for codepoints the primary face doesn't
        // cover. Each is simply skipped when not installed (fontique ignores
        // unknown families), so naming a script whose font is absent is
        // harmless — the text just falls through to the next family.
        "Noto Sans Arabic",     // Arabic (RTL; parley shapes + reorders)
        "Noto Sans Hebrew",     // Hebrew (RTL)
        "Droid Sans Hebrew",    // Hebrew fallback shipped on some distros
        "Noto Sans Devanagari", // Hindi / Marathi / Sanskrit
        "Noto Sans Thai",
        // Arrow / mathematical-operator glyphs. Bundled on most Linux
        // distros via `google-noto-sans-symbols2-fonts` / `-math-`;
        // ignored on systems where they're absent (fontique just
        // skips families it can't find).
        "Noto Sans Symbols 2",
        "Noto Sans Math",
        // Broad cross-platform sans-serif fallbacks. Prefer families
        // that ship Bold/Italic faces — without them, `**bold**` /
        // `_italic_` request weight/style that never resolve and the
        // PDF stays visually regular. DejaVu / Liberation cover Linux;
        // Arial / Segoe UI cover Windows and macOS. Keep Arial Unicode
        // MS and Segoe UI Symbol after those: they cover arrows/math
        // glyphs but typically have no useful Bold/Italic Latin faces.
        "DejaVu Sans",
        "Liberation Sans",
        "Arial",
        "Segoe UI",
        "Helvetica",
        "Arial Unicode MS",
        "Segoe UI Symbol",
        // CJK — deliberately after the symbol / broad-Unicode fallbacks. The
        // pan-CJK fonts greedily cover punctuation, geometric shapes and
        // fullwidth Latin, so listing them earlier would pull ASCII, bullets
        // and arrows into fullwidth CJK forms. Placed here they only catch
        // codepoints nothing else covers — Han, kana, hangul. One family
        // suffices for all three languages; the regional variants differ
        // mainly in preferred Han shapes (Linux: `google-noto-sans-cjk-fonts`).
        "Noto Sans CJK JP",
        "Noto Sans CJK KR",
        "Noto Sans CJK SC",
        "Noto Sans CJK TC",
        "Noto Color Emoji",
    ]
}

pub fn monospace_families(primary: &str) -> Vec<&'static str> {
    // Best-effort: Noto Sans Mono is broadly available.
    let _ = primary; // primary is currently fixed to Noto Sans Mono in style
    vec!["Noto Sans Mono", "Noto Sans"]
}

/// Build a parley Layout for the given text, applying inline ranges.
/// Caller specifies the column width via `max_advance`.
pub fn build_layout(
    text: &str,
    ranges: &[InlineRange],
    style: &TextStyle<'_>,
    max_advance: f32,
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<rgb::Color>,
) -> Layout<rgb::Color> {
    let mut layout = build_unbroken(text, ranges, style, &[], font_cx, layout_cx);
    layout.break_all_lines(Some(max_advance));
    layout.align(Alignment::Start, Default::default());
    layout
}

/// Shared setup for the `build_layout*` family: apply the default text style
/// and the inline ranges, push any inline `boxes` (used by anchored floats as
/// zero-size `CustomOutOfFlow` markers), then build the layout *without*
/// line-breaking it. Callers choose how to break.
fn build_unbroken(
    text: &str,
    ranges: &[InlineRange],
    style: &TextStyle<'_>,
    boxes: &[InlineBox],
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<rgb::Color>,
) -> Layout<rgb::Color> {
    let families: Vec<FontFamilyName<'static>> = style
        .font_families
        .iter()
        .map(|f| FontFamilyName::Named(Cow::Borrowed(*f)))
        .collect();

    let mut builder = layout_cx.ranged_builder(font_cx, text, 1.0, false);
    builder.push_default(StyleProperty::Brush(style.color));
    builder.push_default(StyleProperty::FontFamily(FontFamily::List(Cow::Owned(
        families,
    ))));
    builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(
        style.line_height,
    )));
    builder.push_default(StyleProperty::FontSize(style.font_size));
    builder.push_default(StyleProperty::FontWeight(FontWeight::new(
        style.font_weight,
    )));
    // Break inside a word only when it would otherwise overflow the
    // column (a long URL, part number, file path, …). Words that fit are
    // unaffected, so normal prose wraps exactly as before; this just
    // stops over-wide unbreakable tokens running off the page or out of a
    // table cell / code block.
    builder.push_default(StyleProperty::OverflowWrap(OverflowWrap::Anywhere));
    if style.italic {
        builder.push_default(StyleProperty::FontStyle(FontStyle::Italic));
    }

    for r in ranges {
        match r.prop {
            InlineProp::Bold => builder.push(
                StyleProperty::FontWeight(FontWeight::new(700.0)),
                r.start..r.end,
            ),
            InlineProp::Italic => {
                builder.push(StyleProperty::FontStyle(FontStyle::Italic), r.start..r.end)
            }
            InlineProp::Strikethrough => {
                builder.push(StyleProperty::Strikethrough(true), r.start..r.end)
            }
            InlineProp::Code => {
                // Inline code: switch this range to the monospace family so
                // it stands out from prose. Size and colour stay the body's
                // so it baselines cleanly mid-sentence.
                let mono: Vec<FontFamilyName<'static>> = monospace_families("Noto Sans Mono")
                    .into_iter()
                    .map(|f| FontFamilyName::Named(Cow::Borrowed(f)))
                    .collect();
                builder.push(
                    StyleProperty::FontFamily(FontFamily::List(Cow::Owned(mono))),
                    r.start..r.end,
                );
            }
            InlineProp::Underline { thickness } => {
                // Position is font-derived (drawn from RunMetrics); only the
                // configured thickness is pinned. Colour follows the text
                // brush, which links already tint via `Color`.
                builder.push(StyleProperty::Underline(true), r.start..r.end);
                builder.push(
                    StyleProperty::UnderlineSize(Some(thickness)),
                    r.start..r.end,
                );
            }
            InlineProp::Color(color) => builder.push(StyleProperty::Brush(color), r.start..r.end),
        }
    }

    for b in boxes {
        builder.push_inline_box(b.clone());
    }

    builder.build(text)
}

/// Like [`build_layout`], but breaks lines to flow around a rectangular
/// exclusion at the top of the column (a floated image). While a line's top
/// is above `exclude_height` it is limited to `narrow_width` and shifted to
/// `narrow_x`; once clear of the exclusion, lines use the full `full_width`
/// at x = 0. Each line's chosen `x` lands in its `metrics().inline_min_coord`
/// (parley's per-line origin, distinct from the alignment `offset`); the emit
/// pass adds both to the block origin. This is the wrap-around-image mechanism
/// for `{% float %}`.
#[allow(clippy::too_many_arguments)]
pub fn build_layout_float(
    text: &str,
    ranges: &[InlineRange],
    style: &TextStyle<'_>,
    full_width: f32,
    narrow_width: f32,
    narrow_x: f32,
    exclude_height: f32,
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<rgb::Color>,
) -> Layout<rgb::Color> {
    let mut layout = build_unbroken(text, ranges, style, &[], font_cx, layout_cx);
    let mut breaker = layout.break_lines();
    // Line widths vary, so the uniform layout max-advance must be unset
    // (parley asserts per-line width matches it otherwise). The per-line
    // origin (`line_x`) and width (`line_max_advance`) live on the breaker
    // state, reached via `state_mut()`.
    breaker.state_mut().set_layout_max_advance(f32::INFINITY);
    loop {
        // `committed_y()` is the top of the line about to be laid, so decide
        // narrow-vs-full from it before breaking that line.
        let past_image = breaker.committed_y() as f32 >= exclude_height;
        let st = breaker.state_mut();
        if past_image {
            st.set_line_x(0.0);
            st.set_line_max_advance(full_width);
        } else {
            st.set_line_x(narrow_x);
            st.set_line_max_advance(narrow_width);
        }
        if breaker.break_next().is_none() {
            break;
        }
    }
    breaker.finish();
    layout
}

/// One anchored float for [`build_layout_float_anchored`]: an image pulled to
/// `side_right ? right : left` at the byte `offset` in the text, of the given
/// `width`/`height`. The image occupies no inline space; the surrounding
/// prose narrows on its side for the band it covers.
#[derive(Clone, Copy)]
pub struct FloatSpec {
    pub offset: usize,
    pub side_right: bool,
    pub width: f32,
    pub height: f32,
}

/// One placed float, returned by [`build_layout_float_anchored`] in the same
/// order as the input specs: the image's `x` (relative to the text column's
/// left) and `y` (relative to the layout top).
#[derive(Clone, Copy)]
pub struct FloatPlacement {
    pub x: f32,
    pub y: f32,
}

/// Lay out prose that flows around several anchored floats — the multi-image
/// `{% float %}` (a `float:left` up high, a `float:right` lower down, …). Each
/// spec is pushed as a zero-size `CustomOutOfFlow` inline box at its byte
/// offset; parley yields when the flow reaches one, and this loop drops the
/// float at the *next* line boundary (so image and wrapped text start on the
/// same line), then narrows every following line by whichever floats are still
/// active at its height — on the left (`set_line_x`), the right (reduced
/// `set_line_max_advance`), or both. Returns the laid-out layout and each
/// float's resolved position.
#[allow(clippy::too_many_arguments)]
pub fn build_layout_float_anchored(
    text: &str,
    ranges: &[InlineRange],
    style: &TextStyle<'_>,
    full_width: f32,
    gap: f32,
    specs: &[FloatSpec],
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<rgb::Color>,
) -> (Layout<rgb::Color>, Vec<FloatPlacement>) {
    // Smallest text measure we allow between floats; below this the line would
    // be unreadable, so we clamp (the layout caller also guards up front).
    let min_advance = (style.font_size * 3.0).max(24.0);

    let boxes: Vec<InlineBox> = specs
        .iter()
        .enumerate()
        .map(|(i, s)| InlineBox {
            id: i as u64,
            kind: InlineBoxKind::CustomOutOfFlow,
            index: s.offset,
            width: 0.0,
            height: 0.0,
        })
        .collect();

    let mut layout = build_unbroken(text, ranges, style, &boxes, font_cx, layout_cx);
    let mut placements = vec![FloatPlacement { x: 0.0, y: 0.0 }; specs.len()];

    // Floats currently occupying vertical space: (is-right, bottom_y, width+gap).
    let mut active: Vec<(bool, f32, f32)> = Vec::new();
    // Floats whose anchor was reached on the current line, awaiting the next
    // line boundary before they drop (so they don't overlap the anchor line).
    let mut pending: Vec<usize> = Vec::new();

    let mut breaker = layout.break_lines();
    breaker.state_mut().set_layout_max_advance(f32::INFINITY);
    loop {
        // Set this line's measure from the floats active at its top: shift
        // right past any left float (`set_line_x`) and shorten for any right
        // float (reduced `set_line_max_advance`).
        let y = breaker.committed_y() as f32;
        active.retain(|f| f.1 > y + 0.5);
        let left = active
            .iter()
            .filter(|f| !f.0)
            .map(|f| f.2)
            .fold(0.0_f32, f32::max);
        let right = active
            .iter()
            .filter(|f| f.0)
            .map(|f| f.2)
            .fold(0.0_f32, f32::max);
        let advance = (full_width - left - right).max(min_advance);
        {
            let st = breaker.state_mut();
            st.set_line_x(left);
            st.set_line_max_advance(advance);
        }

        match breaker.break_next() {
            None => break,
            Some(YieldData::InlineBoxBreak(bd)) => {
                // Reached a float anchor: remember it, then step past the
                // zero-size box without consuming inline space (advance
                // unchanged). Geometry is unchanged, so re-setting it above on
                // the next iteration is a no-op until the float actually drops.
                pending.push(bd.inline_box_id as usize);
                breaker
                    .state_mut()
                    .append_inline_box_to_line(bd.advance, 0.0);
            }
            Some(_) => {
                // A line was committed; drop any floats anchored on it at the
                // now-current (next line's) top.
                let ny = breaker.committed_y() as f32;
                for i in pending.drain(..) {
                    let s = specs[i];
                    // Stack below any float already occupying the same side.
                    let top = active
                        .iter()
                        .filter(|f| f.0 == s.side_right)
                        .map(|f| f.1)
                        .fold(ny, f32::max);
                    let x = if s.side_right {
                        full_width - s.width
                    } else {
                        0.0
                    };
                    placements[i] = FloatPlacement { x, y: top };
                    active.push((s.side_right, top + s.height, s.width + gap));
                }
            }
        }
    }
    // Floats anchored on the last line (no trailing line break) drop at the
    // final y so they still render below the text.
    let ny = breaker.committed_y() as f32;
    for i in pending.drain(..) {
        let s = specs[i];
        let top = active
            .iter()
            .filter(|f| f.0 == s.side_right)
            .map(|f| f.1)
            .fold(ny, f32::max);
        let x = if s.side_right {
            full_width - s.width
        } else {
            0.0
        };
        placements[i] = FloatPlacement { x, y: top };
        active.push((s.side_right, top + s.height, s.width + gap));
    }
    breaker.finish();

    (layout, placements)
}

/// Like [`build_layout`] but with a caller-chosen text alignment.
/// Used for header/footer slots so a multi-line center slot has every
/// line centred (and a multi-line right slot stays right-aligned).
pub fn build_layout_aligned(
    text: &str,
    ranges: &[InlineRange],
    style: &TextStyle<'_>,
    max_advance: f32,
    alignment: Alignment,
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<rgb::Color>,
) -> Layout<rgb::Color> {
    let mut layout = build_layout(text, ranges, style, max_advance, font_cx, layout_cx);
    layout.align(alignment, Default::default());
    layout
}

/// Emit a parley `Layout` to the krilla surface.
///
/// `origin_x` / `origin_y_top` is where this slice's first drawn line
/// should appear. `line_range` selects which lines to draw (default
/// `0..n` for the whole layout). `skip_y` is the cumulative height of
/// lines preceding `line_range.start`; subtracting it from each
/// baseline computation puts line `line_range.start` at `origin_y_top`.
///
/// Each line's per-line `metrics().offset` is honoured — parley sets
/// this when `Alignment::Center` or `Alignment::End` is used so
/// individual lines start at the right x within the layout's advance
/// width. For default `Alignment::Start` text the offset is 0. The
/// line's `inline_min_coord` (its per-line origin, set by `{% float %}`
/// via `set_line_x`) is added too; it is 0 for ordinary paragraphs.
#[allow(clippy::too_many_arguments)]
pub fn emit_layout(
    surface: &mut krilla::surface::Surface<'_>,
    layout: &Layout<rgb::Color>,
    text: &str,
    origin_x: f32,
    origin_y_top: f32,
    font_cache: &mut HashMap<u64, Font>,
    line_range: std::ops::Range<usize>,
    skip_y: f32,
) {
    for (i, line) in layout.lines().enumerate() {
        if i < line_range.start {
            continue;
        }
        if i >= line_range.end {
            break;
        }
        let baseline_y = origin_y_top - skip_y + line.metrics().baseline;
        let mut x = origin_x + line.metrics().offset + line.metrics().inline_min_coord;

        for run in line.runs() {
            let mut cur_x = x;
            let parley_font = run.font().clone();
            let (font_data, font_id) = parley_font.data.into_raw_parts();
            let font_index = parley_font.index;
            let krilla_font = font_cache
                .entry(font_id)
                .or_insert_with(|| Font::new(font_data.into(), font_index).unwrap())
                .clone();
            let run_size = run.font_size();

            let mut cur_style: Option<u16> = None;
            let mut glyphs = Vec::<KrillaGlyph>::new();

            for cluster in run.visual_clusters() {
                if cluster.is_ligature_continuation() {
                    if let Some(g) = glyphs.last_mut() {
                        g.text_range.end = cluster.text_range().end;
                    }
                    continue;
                }
                for glyph in cluster.glyphs() {
                    let glyph_style = glyph.style_index;
                    if let Some(prev) = cur_style {
                        if prev != glyph_style {
                            cur_style = Some(glyph_style);
                            surface.set_fill(Some(Fill {
                                paint: layout.styles()[prev as usize].brush.into(),
                                opacity: NormalizedF32::ONE,
                                rule: Default::default(),
                            }));
                            surface.draw_glyphs(
                                Point::from_xy(cur_x, baseline_y),
                                &glyphs,
                                krilla_font.clone(),
                                text,
                                run_size,
                                false,
                            );
                            glyphs.clear();
                            cur_x = x;
                        }
                    } else {
                        cur_style = Some(glyph_style);
                    }
                    glyphs.push(KrillaGlyph::new(
                        GlyphId::new(glyph.id),
                        glyph.advance / run_size,
                        glyph.x / run_size,
                        glyph.y / run_size,
                        0.0,
                        cluster.text_range(),
                        None,
                    ));
                    x += glyph.advance;
                }
            }

            if !glyphs.is_empty() {
                surface.set_fill(Some(Fill {
                    paint: layout.styles()[cur_style.unwrap() as usize].brush.into(),
                    opacity: NormalizedF32::ONE,
                    rule: Default::default(),
                }));
                surface.draw_glyphs(
                    Point::from_xy(cur_x, baseline_y),
                    &glyphs,
                    krilla_font.clone(),
                    text,
                    run_size,
                    false,
                );
            }
        }
    }
}

/// One contiguous tagged-content section produced by
/// [`emit_layout_segmented`]. Plain (non-link) text on a line is
/// usually one segment; a link splits the line into up to three:
/// before-link, the link itself, after-link. A link that wraps onto a
/// second line produces a fresh segment per line so each line's
/// annotation rect gets its own structure-tree leaf.
pub struct TextSegment {
    pub id: Identifier,
    pub line_idx: usize,
    /// Index into the block's `links` slice when this segment falls
    /// inside a link; `None` when it is plain text.
    pub link_idx_in_block: Option<usize>,
}

/// Same as [`emit_layout`] but breaks tagged content into segments at
/// every link boundary so each link can be wrapped in its own PDF/UA
/// `Link` tag group. Each returned segment owns one tagged-content
/// identifier; the caller stitches them into `Span` (plain) or `Link`
/// (link) groups as appropriate. Tagging is unconditional here —
/// callers must only use this function when tagging is enabled.
#[allow(clippy::too_many_arguments)]
pub fn emit_layout_segmented(
    surface: &mut krilla::surface::Surface<'_>,
    layout: &Layout<rgb::Color>,
    text: &str,
    origin_x: f32,
    origin_y_top: f32,
    font_cache: &mut HashMap<u64, Font>,
    line_range: std::ops::Range<usize>,
    skip_y: f32,
    links: &[LinkRange],
) -> Vec<TextSegment> {
    let mut segments = Vec::<TextSegment>::new();
    // Currently-open tagged section, if any: (line_idx, link_idx, id).
    let mut cur_seg: Option<(usize, Option<usize>, Identifier)> = None;

    for (i, line) in layout.lines().enumerate() {
        if i < line_range.start {
            continue;
        }
        if i >= line_range.end {
            break;
        }
        let baseline_y = origin_y_top - skip_y + line.metrics().baseline;
        let mut x = origin_x + line.metrics().offset + line.metrics().inline_min_coord;

        for run in line.runs() {
            let mut cur_x = x;
            let parley_font = run.font().clone();
            let (font_data, font_id) = parley_font.data.into_raw_parts();
            let font_index = parley_font.index;
            let krilla_font = font_cache
                .entry(font_id)
                .or_insert_with(|| Font::new(font_data.into(), font_index).unwrap())
                .clone();
            let run_size = run.font_size();

            let mut cur_style: Option<u16> = None;
            let mut glyphs = Vec::<KrillaGlyph>::new();

            for cluster in run.visual_clusters() {
                if cluster.is_ligature_continuation() {
                    if let Some(g) = glyphs.last_mut() {
                        g.text_range.end = cluster.text_range().end;
                    }
                    continue;
                }

                let cluster_range = cluster.text_range();
                let new_link_idx = links
                    .iter()
                    .position(|l| cluster_range.start < l.end && cluster_range.end > l.start);

                // Switch sections at every link-state change, plus at
                // every line boundary while we're inside a link (each
                // line of a wrapped link gets its own annotation rect
                // and so its own Span identifier). Plain text spans
                // line breaks within a single section to keep the
                // structure tree compact.
                let need_switch = match cur_seg {
                    None => true,
                    Some((prev_line, prev_link, _)) => {
                        prev_link != new_link_idx || (prev_link.is_some() && prev_line != i)
                    }
                };

                if need_switch {
                    // Flush any in-flight glyphs (they belong to the
                    // currently-open section, drawn at the current line's
                    // baseline — both still valid here).
                    if let Some(brush) = cur_style.take()
                        && !glyphs.is_empty()
                    {
                        surface.set_fill(Some(Fill {
                            paint: layout.styles()[brush as usize].brush.into(),
                            opacity: NormalizedF32::ONE,
                            rule: Default::default(),
                        }));
                        surface.draw_glyphs(
                            Point::from_xy(cur_x, baseline_y),
                            &glyphs,
                            krilla_font.clone(),
                            text,
                            run_size,
                            false,
                        );
                        glyphs.clear();
                        cur_x = x;
                    }
                    if let Some((prev_line, prev_link, id)) = cur_seg.take() {
                        surface.end_tagged();
                        segments.push(TextSegment {
                            id,
                            line_idx: prev_line,
                            link_idx_in_block: prev_link,
                        });
                    }
                    let id = surface.start_tagged(ContentTag::Span(SpanTag::empty()));
                    cur_seg = Some((i, new_link_idx, id));
                }

                for glyph in cluster.glyphs() {
                    let glyph_style = glyph.style_index;
                    if let Some(prev) = cur_style {
                        if prev != glyph_style {
                            surface.set_fill(Some(Fill {
                                paint: layout.styles()[prev as usize].brush.into(),
                                opacity: NormalizedF32::ONE,
                                rule: Default::default(),
                            }));
                            surface.draw_glyphs(
                                Point::from_xy(cur_x, baseline_y),
                                &glyphs,
                                krilla_font.clone(),
                                text,
                                run_size,
                                false,
                            );
                            glyphs.clear();
                            cur_x = x;
                            cur_style = Some(glyph_style);
                        }
                    } else {
                        cur_style = Some(glyph_style);
                    }
                    glyphs.push(KrillaGlyph::new(
                        GlyphId::new(glyph.id),
                        glyph.advance / run_size,
                        glyph.x / run_size,
                        glyph.y / run_size,
                        0.0,
                        cluster.text_range(),
                        None,
                    ));
                    x += glyph.advance;
                }
            }

            if !glyphs.is_empty() {
                surface.set_fill(Some(Fill {
                    paint: layout.styles()[cur_style.unwrap() as usize].brush.into(),
                    opacity: NormalizedF32::ONE,
                    rule: Default::default(),
                }));
                surface.draw_glyphs(
                    Point::from_xy(cur_x, baseline_y),
                    &glyphs,
                    krilla_font.clone(),
                    text,
                    run_size,
                    false,
                );
            }
        }
    }

    if let Some((line_idx, link_idx, id)) = cur_seg.take() {
        surface.end_tagged();
        segments.push(TextSegment {
            id,
            line_idx,
            link_idx_in_block: link_idx,
        });
    }

    segments
}
