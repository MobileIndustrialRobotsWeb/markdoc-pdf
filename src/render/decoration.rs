//! Page-level decoration: headers, footers, page numbers.
//!
//! Drawn during emit, after body content, before the surface is
//! finished. Each decoration band has three slots — left, center,
//! right — populated from template strings via `TemplateContext`.

use std::collections::HashMap;

use std::sync::Arc;

use krilla::geom::{PathBuilder, Point, Size, Transform};
use krilla::image::Image as KrillaImage;
use krilla::num::NormalizedF32;
use krilla::paint::Stroke;
use krilla::tagging::{Artifact, ArtifactType, ContentTag};
use krilla::text::Font;
use krilla_svg::{SurfaceExt, SvgSettings};
use parley::{FontContext, LayoutContext};
use usvg::Tree as SvgTree;

use parley::layout::Alignment;

use crate::assets::{AssetResolver, MediaFormat, sniff_format};

use super::TemplateContext;
use super::style::{HeaderFooterStyle, LogoSpec, Style};
use super::text::{TextStyle, build_layout, build_layout_aligned, default_families, emit_layout};

/// A decoded image / SVG asset ready to draw. Raster formats are
/// decoded to a krilla Image; SVG sources parse into a usvg Tree shared
/// via Arc so the same asset can be redrawn cheaply across pages. Used
/// for header/footer logos, watermarks, and callout icons.
#[derive(Clone)]
pub enum DecodedMedia {
    Raster(KrillaImage),
    Svg(Arc<SvgTree>),
}

/// In-memory cache of decoded media keyed by their asset URI so the
/// renderer pays decoding cost once per document, not once per page (or
/// per use site). `None` entries record load/decoder failures so we
/// don't retry the same broken URI repeatedly.
pub type MediaCache = HashMap<String, Option<DecodedMedia>>;

#[allow(clippy::too_many_arguments)]
pub fn emit_header(
    surface: &mut krilla::surface::Surface<'_>,
    style: &Style,
    header: &HeaderFooterStyle,
    tctx: &TemplateContext<'_>,
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<krilla::color::rgb::Color>,
    font_cache: &mut HashMap<u64, Font>,
    assets: &dyn AssetResolver,
    media_cache: &mut MediaCache,
    tagged: bool,
) {
    let text_top = header.margin_from_edge;
    if tagged {
        surface.start_tagged(ContentTag::Artifact(Artifact::with_kind(
            ArtifactType::Header,
        )));
    }
    emit_three_slots(
        surface,
        style,
        header,
        tctx,
        text_top,
        font_cx,
        layout_cx,
        font_cache,
        assets,
        media_cache,
    );
    if header.rule {
        let rule_y = text_top + header.font_size * 1.2 + header.rule_gap;
        draw_rule(surface, style, header, rule_y);
    }
    if tagged {
        surface.end_tagged();
    }
}

#[allow(clippy::too_many_arguments)]
pub fn emit_footer(
    surface: &mut krilla::surface::Surface<'_>,
    style: &Style,
    footer: &HeaderFooterStyle,
    tctx: &TemplateContext<'_>,
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<krilla::color::rgb::Color>,
    font_cache: &mut HashMap<u64, Font>,
    assets: &dyn AssetResolver,
    media_cache: &mut MediaCache,
    tagged: bool,
) {
    let text_height = footer.font_size * 1.2;
    let text_top = style.page_height - footer.margin_from_edge - text_height;
    if tagged {
        surface.start_tagged(ContentTag::Artifact(Artifact::with_kind(
            ArtifactType::Footer,
        )));
    }
    if footer.rule {
        let rule_y = text_top - footer.rule_gap;
        draw_rule(surface, style, footer, rule_y);
    }
    emit_three_slots(
        surface,
        style,
        footer,
        tctx,
        text_top,
        font_cx,
        layout_cx,
        font_cache,
        assets,
        media_cache,
    );
    if tagged {
        surface.end_tagged();
    }
}

/// Stamp a QR code encoding the resolved `spec.value` in a bottom corner of
/// the page (left / centre / right per `spec.align`), above a `spec.label`
/// caption. Silently does nothing
/// when the value template leaves an unresolved `{token}` (e.g. the document
/// has no `documentNumber`) or resolves to empty — so it is safe to enable
/// document-wide. Tagged as an artifact: it is production metadata, not
/// reading-order content.
#[allow(clippy::too_many_arguments)]
pub fn emit_last_page_qr(
    surface: &mut krilla::surface::Surface<'_>,
    style: &Style,
    spec: &super::style::LastPageQr,
    tctx: &TemplateContext<'_>,
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<krilla::color::rgb::Color>,
    font_cache: &mut HashMap<u64, Font>,
    tagged: bool,
) {
    if spec.size <= 0.0 {
        return;
    }
    let value = tctx.substitute(&spec.value);
    let value = value.trim();
    if value.is_empty() || value.contains('{') {
        return; // unresolved field or empty — nothing to stamp
    }
    let dark = format!(
        "#{:02x}{:02x}{:02x}",
        spec.color.r, spec.color.g, spec.color.b
    );
    let Some(svg) = super::block::qr_svg_string(value, &spec.ecl, 4, &dark, "#ffffff") else {
        return; // value too long to encode
    };
    let Ok(tree) = SvgTree::from_data(svg.as_bytes(), &usvg::Options::default()) else {
        return;
    };
    let Some(qr_size) = Size::from_wh(spec.size, spec.size) else {
        return;
    };

    // Optional human-readable caption below the code.
    let label = tctx.substitute(&spec.label);
    let label = label.trim();
    let has_label = !label.is_empty() && !label.contains('{');
    let label_gap = if has_label { 3.0 } else { 0.0 };
    let label_h = if has_label {
        spec.label_font_size * 1.25
    } else {
        0.0
    };

    // Vertically anchored so the caption's bottom sits `margin_bottom` above
    // the page edge. Horizontally, `align` picks the corner: left / centre /
    // right (default), inset by the matching margin (`center` ignores both).
    let qr_x = match spec.align.trim().to_ascii_lowercase().as_str() {
        "left" => spec.margin_left,
        "center" | "centre" => (style.page_width - spec.size) / 2.0,
        _ => style.page_width - spec.margin_right - spec.size,
    };
    let qr_y = style.page_height - spec.margin_bottom - label_h - label_gap - spec.size;

    if tagged {
        surface.start_tagged(ContentTag::Artifact(Artifact::with_kind(
            ArtifactType::Other,
        )));
    }
    surface.push_transform(&Transform::from_translate(qr_x, qr_y));
    surface.draw_svg(&tree, qr_size, SvgSettings::default());
    surface.pop();

    if has_label {
        let text_style = TextStyle {
            font_size: spec.label_font_size,
            font_weight: 400.0,
            line_height: 1.2,
            color: spec.color.into(),
            font_families: default_families(),
            italic: false,
        };
        let layout = build_layout_aligned(
            label,
            &[],
            &text_style,
            spec.size,
            Alignment::Center,
            font_cx,
            layout_cx,
        );
        let label_y = qr_y + spec.size + label_gap;
        let lines = layout.lines().count().min(1);
        emit_layout(
            surface,
            &layout,
            label,
            qr_x,
            label_y,
            font_cache,
            0..lines,
            0.0,
        );
    }
    if tagged {
        surface.end_tagged();
    }
}

/// Draw the notice masthead at the top of a page, starting at `band_top`.
/// Layout: logo top-left with a subtitle beneath it and a wrapping
/// disclaimer below that; on the right, a label right-aligned just above
/// the closing rule with its icon centred over it; a note line on the
/// left, and a full-width rule closing the band. Every piece is optional.
#[allow(clippy::too_many_arguments)]
pub fn emit_notice_banner(
    surface: &mut krilla::surface::Surface<'_>,
    style: &Style,
    banner: &super::style::NoticeBanner,
    band_top: f32,
    tctx: &TemplateContext<'_>,
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<krilla::color::rgb::Color>,
    font_cache: &mut HashMap<u64, Font>,
    assets: &dyn AssetResolver,
    media_cache: &mut MediaCache,
    tagged: bool,
) {
    let left = style.margin_x;
    let right = style.page_width - style.margin_x;
    let band_bottom = band_top + banner.height;

    if tagged {
        surface.start_tagged(ContentTag::Artifact(Artifact::with_kind(
            ArtifactType::Header,
        )));
    }

    // ── The icon label is built up front so the right-hand icon can be
    //    centred over it. The label is drawn later, once the closing
    //    rule's y is known, so it sits just above the rule. ──────────────
    let label_text = tctx.substitute(&banner.label);
    let label_layout = (!label_text.trim().is_empty()).then(|| {
        let st = banner_text_style(banner.label_color, banner.label_font_size);
        build_layout(&label_text, &[], &st, right - left, font_cx, layout_cx)
    });
    let label_w = label_layout
        .as_ref()
        .and_then(|l| l.lines().next())
        .map(|l| l.metrics().advance)
        .unwrap_or(0.0);
    // The label is right-aligned to the margin.
    let label_x = (right - label_w).max(left);

    // ── Right icon: centred over its label (or flush-right when there is
    //    no label). Its x and the disclaimer's wrap width are fixed now,
    //    but it is drawn later — once the label's y is known — so it can
    //    be lowered toward the label. ──────────────────────────────────
    // The disclaimer wraps up to `right_block_left`, kept clear of BOTH the
    // right-hand icon AND its label — the label is often wider than the icon
    // and reaches further left, so wrapping only around the icon would let the
    // disclaimer run into the label.
    let mut right_block_left = right;
    if label_layout.is_some() {
        right_block_left = right_block_left.min(label_x - 12.0);
    }
    let mut icon_x = 0.0;
    let mut icon_h = 0.0;
    if let Some(icon) = &banner.icon {
        icon_x = if label_layout.is_some() {
            (label_x + (label_w - icon.width) / 2.0).clamp(left, right - icon.width)
        } else {
            right - icon.width
        };
        right_block_left = right_block_left.min(icon_x - 12.0);
        icon_h = icon.height;
    }

    // ── Left column: logo, subtitle, disclaimer ────────────────────────
    let mut y = band_top;
    if let Some(logo) = &banner.logo {
        draw_logo(surface, logo, left, band_top, assets, media_cache);
        y = band_top + logo.height;
    }
    if !banner.logo_subtitle.trim().is_empty() {
        let s = tctx.substitute(&banner.logo_subtitle);
        let st = banner_text_style(banner.logo_subtitle_color, banner.logo_subtitle_font_size);
        let layout = build_layout(&s, &[], &st, right - left, font_cx, layout_cx);
        emit_layout(surface, &layout, &s, left, y + 1.0, font_cache, 0..1, 0.0);
        y += banner.logo_subtitle_font_size * 1.4;
    }
    if !banner.disclaimer.trim().is_empty() {
        let s = tctx.substitute(&banner.disclaimer);
        let st = banner_text_style(banner.disclaimer_color, banner.disclaimer_font_size);
        let avail = (right_block_left - left).max(40.0);
        let layout = build_layout(&s, &[], &st, avail, font_cx, layout_cx);
        let lines = layout
            .lines()
            .count()
            .min(banner.disclaimer_max_lines.max(1) as usize);
        y += 4.0;
        emit_layout(surface, &layout, &s, left, y, font_cache, 0..lines, 0.0);
        y += lines as f32 * banner.disclaimer_font_size * 1.3;
    }

    // ── Note line, then the closing rule directly beneath it ───────────
    // The note flows under the disclaimer so the two never overlap,
    // regardless of how many lines the disclaimer wrapped to.
    if !banner.note.trim().is_empty() {
        let s = tctx.substitute(&banner.note);
        let st = banner_text_style(banner.note_color, banner.note_font_size);
        let layout = build_layout(&s, &[], &st, right - left, font_cx, layout_cx);
        emit_layout(surface, &layout, &s, left, y + 2.0, font_cache, 0..1, 0.0);
        y += banner.note_font_size * 1.3 + 2.0;
    }
    // Close the band with a rule below the flowed content (clamped to the
    // reserved band bottom so it never crosses into the body).
    let rule_y = (y + 3.0).min(band_bottom);

    // The label sits just above the rule (and clear of the icon above it).
    let line_h = banner.label_font_size * 1.25;
    let label_top = (rule_y - line_h - 3.0).max(band_top + icon_h + 2.0);

    // Sit the icon just above the label with a small, consistent gap so the
    // two read as a group — regardless of how tall the disclaimer made the
    // band. Clamped so the icon never rises above the band top.
    if let Some(icon) = &banner.icon {
        let icon_y = (label_top - icon.height - 6.0).max(band_top);
        draw_logo(surface, icon, icon_x, icon_y, assets, media_cache);
    }

    if banner.rule {
        stroke_hline(
            surface,
            left,
            right,
            rule_y,
            banner.rule_color.into(),
            banner.rule_thickness,
        );
    }

    // ── Icon label: right-aligned to the margin (with the icon centred
    //    over it), sitting just above the rule. ─────────────────────────
    if let Some(layout) = &label_layout {
        emit_layout(
            surface,
            layout,
            &label_text,
            label_x,
            label_top,
            font_cache,
            0..1,
            0.0,
        );
    }

    if tagged {
        surface.end_tagged();
    }
}

fn banner_text_style(color: super::style::ColorRgb, font_size: f32) -> TextStyle<'static> {
    TextStyle {
        font_size,
        font_weight: 400.0,
        line_height: 1.25,
        color: color.into(),
        font_families: default_families(),
        italic: false,
    }
}

/// Stroke a horizontal line from `x0` to `x1` at `y`.
fn stroke_hline(
    surface: &mut krilla::surface::Surface<'_>,
    x0: f32,
    x1: f32,
    y: f32,
    color: krilla::color::rgb::Color,
    thickness: f32,
) {
    let mut pb = PathBuilder::new();
    pb.move_to(x0, y);
    pb.line_to(x1, y);
    if let Some(path) = pb.finish() {
        surface.set_stroke(Some(Stroke {
            paint: color.into(),
            width: thickness,
            opacity: NormalizedF32::ONE,
            ..Default::default()
        }));
        surface.draw_path(&path);
        surface.set_stroke(None);
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_three_slots(
    surface: &mut krilla::surface::Surface<'_>,
    style: &Style,
    spec: &HeaderFooterStyle,
    tctx: &TemplateContext<'_>,
    text_top: f32,
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<krilla::color::rgb::Color>,
    font_cache: &mut HashMap<u64, Font>,
    assets: &dyn AssetResolver,
    media_cache: &mut MediaCache,
) {
    let body_left = style.margin_x;
    let body_right = style.page_width - style.margin_x;
    let body_width = body_right - body_left;
    let text_style = TextStyle {
        font_size: spec.font_size,
        font_weight: 400.0,
        line_height: 1.2,
        color: spec.color.into(),
        font_families: default_families(),
        italic: false,
    };

    let max_lines = spec.max_lines.max(1) as usize;
    let (left, center, right) = spec.resolved_slots(tctx.page, tctx.chapter);

    // LEFT slot: logo + text can coexist — logo first, text shifts
    // right by `logo.width + logo.gap`. CENTER slot remains exclusive
    // (centring text alongside a logo would need width measurement
    // and an asymmetric layout — out of scope for v1).
    let left_text_x = match &spec.logo_left {
        Some(logo) => {
            draw_logo(surface, logo, body_left, text_top, assets, media_cache);
            body_left + logo.width + logo.gap
        }
        None => body_left,
    };
    if !left.is_empty() {
        let s = tctx.substitute(&left);
        let avail = (body_right - left_text_x).max(0.0);
        let layout = build_layout(&s, &[], &text_style, avail, font_cx, layout_cx);
        let lines = layout.lines().count().min(max_lines);
        emit_layout(
            surface,
            &layout,
            &s,
            left_text_x,
            text_top,
            font_cache,
            0..lines,
            0.0,
        );
    }
    if let Some(logo) = &spec.logo_center {
        let x = body_left + (body_width - logo.width) * 0.5;
        draw_logo(surface, logo, x, text_top, assets, media_cache);
    } else if !center.is_empty() {
        let s = tctx.substitute(&center);
        let layout = build_layout_aligned(
            &s,
            &[],
            &text_style,
            body_width,
            Alignment::Center,
            font_cx,
            layout_cx,
        );
        let lines = layout.lines().count().min(max_lines);
        // Aligned at body_left so parley's per-line offsets land each
        // line at the band's true centre.
        emit_layout(
            surface,
            &layout,
            &s,
            body_left,
            text_top,
            font_cache,
            0..lines,
            0.0,
        );
    }
    let right_text_end_x = match &spec.logo_right {
        Some(logo) => {
            draw_logo(
                surface,
                logo,
                body_right - logo.width,
                text_top,
                assets,
                media_cache,
            );
            body_right - logo.width - logo.gap
        }
        None => body_right,
    };
    if !right.is_empty() {
        let s = tctx.substitute(&right);
        // Right-align the text into the printable column, then trim
        // the right edge by the logo's footprint so the text stays
        // clear of the image. Done by building the layout into the
        // narrower advance and emitting at body_left.
        let avail = (right_text_end_x - body_left).max(0.0);
        let layout = build_layout_aligned(
            &s,
            &[],
            &text_style,
            avail,
            Alignment::End,
            font_cx,
            layout_cx,
        );
        let lines = layout.lines().count().min(max_lines);
        emit_layout(
            surface,
            &layout,
            &s,
            body_left,
            text_top,
            font_cache,
            0..lines,
            0.0,
        );
    }
}

/// Decode the logo (with caching), then draw it at the requested
/// position and size. Failures are silently ignored — a broken logo
/// shouldn't block the entire page from rendering.
fn draw_logo(
    surface: &mut krilla::surface::Surface<'_>,
    logo: &LogoSpec,
    x: f32,
    y: f32,
    assets: &dyn AssetResolver,
    media_cache: &mut MediaCache,
) {
    if logo.src.is_empty() || logo.width <= 0.0 || logo.height <= 0.0 {
        return;
    }
    let entry = media_cache
        .entry(logo.src.clone())
        .or_insert_with(|| decode_media(&logo.src, assets));
    let Some(decoded) = entry.clone() else { return };
    let Some(size) = Size::from_wh(logo.width, logo.height) else {
        return;
    };
    surface.push_transform(&Transform::from_translate(x, y));
    match decoded {
        DecodedMedia::Raster(image) => surface.draw_image(image, size),
        DecodedMedia::Svg(tree) => {
            surface.draw_svg(tree.as_ref(), size, SvgSettings::default());
        }
    }
    surface.pop();
}

/// Decode an image / SVG asset into a [`DecodedMedia`] via the resolver.
/// Shared by the logo, watermark, and callout-icon paths. Returns
/// `None` on fetch or decode failure.
pub(super) fn decode_media(src: &str, assets: &dyn AssetResolver) -> Option<DecodedMedia> {
    let bytes = assets.fetch(src).ok()?;
    let format = sniff_format(&bytes);
    match format {
        MediaFormat::Png => KrillaImage::from_png(bytes.into(), false)
            .ok()
            .map(DecodedMedia::Raster),
        MediaFormat::Jpeg => KrillaImage::from_jpeg(bytes.into(), false)
            .ok()
            .map(DecodedMedia::Raster),
        MediaFormat::Gif => KrillaImage::from_gif(bytes.into(), false)
            .ok()
            .map(DecodedMedia::Raster),
        MediaFormat::Webp => KrillaImage::from_webp(bytes.into(), false)
            .ok()
            .map(DecodedMedia::Raster),
        MediaFormat::Svg => {
            let opts = usvg::Options::default();
            SvgTree::from_data(&bytes, &opts)
                .ok()
                .map(|t| DecodedMedia::Svg(Arc::new(t)))
        }
        _ => None,
    }
}

/// Draw the watermark beneath body content. Wrapped in an Artifact
/// content tag so screen readers ignore it. No-ops when the watermark
/// is image-based but the asset can't be decoded.
#[allow(clippy::too_many_arguments)]
pub fn emit_watermark(
    surface: &mut krilla::surface::Surface<'_>,
    style: &Style,
    watermark: &super::style::Watermark,
    page_idx: usize,
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<krilla::color::rgb::Color>,
    font_cache: &mut HashMap<u64, Font>,
    assets: &dyn AssetResolver,
    media_cache: &mut MediaCache,
    tagged: bool,
) {
    if watermark.skip_first_page && page_idx == 0 {
        return;
    }
    let opacity =
        NormalizedF32::new(watermark.opacity.clamp(0.0, 1.0)).unwrap_or(NormalizedF32::ONE);
    if tagged {
        surface.start_tagged(ContentTag::Artifact(Artifact::with_kind(
            krilla::tagging::ArtifactType::Page,
        )));
    }
    match &watermark.kind {
        super::style::WatermarkKind::Image(img) => {
            // Reuse the logo decoder + cache — same kind of asset.
            let logo = LogoSpec {
                src: img.src.clone(),
                id: String::new(),
                width: img.width,
                height: img.height,
                gap: 0.0,
                ..Default::default()
            };
            let entry = media_cache
                .entry(img.src.clone())
                .or_insert_with(|| decode_media(&img.src, assets));
            let Some(decoded) = entry.clone() else {
                if tagged {
                    surface.end_tagged();
                }
                return;
            };
            let Some(size) = Size::from_wh(logo.width, logo.height) else {
                if tagged {
                    surface.end_tagged();
                }
                return;
            };
            surface.push_transform(&Transform::from_translate(img.x, img.y));
            surface.push_opacity(opacity);
            match decoded {
                DecodedMedia::Raster(image) => surface.draw_image(image, size),
                DecodedMedia::Svg(tree) => {
                    surface.draw_svg(tree.as_ref(), size, SvgSettings::default());
                }
            }
            surface.pop();
            surface.pop();
        }
        super::style::WatermarkKind::Text(t) => {
            if t.text.trim().is_empty() {
                if tagged {
                    surface.end_tagged();
                }
                return;
            }
            let text_style = TextStyle {
                font_size: t.font_size,
                font_weight: 700.0,
                line_height: 1.0,
                color: t.color.into(),
                font_families: default_families(),
                italic: false,
            };
            // Use Start alignment + manual centring: parley's `offset` is 0 so
            // emit_layout draws at our translated origin verbatim. Centring
            // is computed below from the line's `advance` and applied via a
            // translate transform, because the watermark has to land at the
            // page centre, not the column centre — Center alignment would
            // give us the wrong reference point.
            let layout = build_layout_aligned(
                &t.text,
                &[],
                &text_style,
                10_000.0,
                Alignment::Start,
                font_cx,
                layout_cx,
            );
            // Page centre.
            let cx = style.page_width * 0.5;
            let cy = style.page_height * 0.5;
            let advance = layout
                .lines()
                .next()
                .map(|l| l.metrics().advance)
                .unwrap_or(0.0);
            let baseline = layout
                .lines()
                .next()
                .map(|l| l.metrics().baseline)
                .unwrap_or(t.font_size);
            let half_w = advance * 0.5;
            // Approximate visual centre offset = baseline − 0.35×em.
            // Good enough for a decorative backdrop.
            let half_h_offset = baseline - t.font_size * 0.35;
            // Stack: rotate-around-page-centre, then translate so the
            // text's centre lands on (cx, cy) before rotation. Two
            // pushes give us R · T composition (PDF concatenates on
            // the left, so the inner push runs first when drawing).
            surface.push_transform(&Transform::from_rotate_at(t.rotation_deg, cx, cy));
            surface.push_transform(&Transform::from_translate(cx - half_w, cy - half_h_offset));
            surface.push_opacity(opacity);
            let lines = layout.lines().count();
            emit_layout(
                surface,
                &layout,
                &t.text,
                0.0,
                0.0,
                font_cache,
                0..lines,
                0.0,
            );
            surface.pop();
            surface.pop();
            surface.pop();
        }
    }
    if tagged {
        surface.end_tagged();
    }
}

fn draw_rule(
    surface: &mut krilla::surface::Surface<'_>,
    style: &Style,
    spec: &HeaderFooterStyle,
    y: f32,
) {
    let left = style.margin_x;
    let right = style.page_width - style.margin_x;
    let mut pb = PathBuilder::new();
    pb.move_to(left, y);
    pb.line_to(right, y);
    if let Some(path) = pb.finish() {
        let color: krilla::color::rgb::Color = spec.rule_color.into();
        surface.set_stroke(Some(Stroke {
            paint: color.into(),
            width: spec.rule_thickness,
            opacity: NormalizedF32::ONE,
            ..Default::default()
        }));
        surface.draw_path(&path);
        surface.set_stroke(None);
    }
    // Suppress unused-Point/PathBuilder warnings when nothing draws.
    let _ = Point::from_xy(0.0, 0.0);
    let _ = PathBuilder::new();
}
