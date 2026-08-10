//! PDF rendering for transformed Markdoc documents.
//!
//! Three-pass pipeline:
//!   1. **layout** ([`block::layout_document`]) — walk the rendered tree,
//!      lay out each block via parley, return a flat `Vec<Block>` with
//!      heights baked in.
//!   2. **paginate** ([`paginate::paginate`]) — greedy split of blocks
//!      into pages so the running height stays under the page budget.
//!   3. **emit** ([`emit::emit_blocks`]) — open one krilla page per group
//!      and draw the blocks.

mod block;
mod coverpage;
mod crossref_labels;
mod decoration;
mod emit;
mod highlight;
mod hyphen;
mod inline;
mod paginate;
pub mod style;
mod text;

use std::collections::HashMap;

use krilla::Document;
use krilla::SerializeSettings;
use krilla::action::{Action, LinkAction};
use krilla::annotation::{Annotation, LinkAnnotation, Target};
use krilla::configure::{Accessibility, Archival, ConfigurationBuilder};
use krilla::destination::XyzDestination;
use krilla::geom::{Point, Rect};
use krilla::metadata::Metadata;
use krilla::outline::{Outline, OutlineNode};
use krilla::page::PageSettings;
use krilla::tagging::{Tag, TagGroup, TagKind, TagTree, kind};
use krilla::text::Font;
use markdoc::types::RenderableTreeNode;
use parley::{FontContext, LayoutContext};

use style::{PdfExportProfile, TocPosition};

use crate::assets::AssetResolver;

use block::LayoutCtx;
pub use style::{Style, StyleLoadError};

/// Caller-supplied context that the renderer can interpolate into
/// header/footer template strings and surface as PDF document metadata.
#[derive(Debug, Clone, Default)]
pub struct RenderContext {
    /// Document title — substituted as `{title}` in header/footer
    /// templates and emitted as the PDF's `/Title` metadata.
    pub title: String,
    /// Document language code (e.g. `"en-us"`). Required by PDF/A.
    pub language: Option<String>,
    /// Document description / abstract.
    pub description: Option<String>,
    /// Authors (combined into PDF `/Author` metadata).
    pub authors: Vec<String>,
    /// Application that created the document.
    pub creator: Option<String>,
    /// Library / pipeline that wrote the PDF bytes (`/Producer`).
    /// Defaults to krilla's own string when `None`. Set this to
    /// e.g. `"markdoc-pdf 0.1.0"` so support tooling can identify
    /// which version of the renderer produced a given file.
    pub producer: Option<String>,
    /// Document creation date for the PDF metadata. If `None`, the
    /// renderer falls back to the current system time so PDF/A docs
    /// always carry a valid `/CreationDate`. Use `dates::parse_iso`
    /// to convert from a frontmatter ISO date string, or
    /// `dates::now()` to stamp the build time.
    pub creation_date: Option<krilla::metadata::DateTime>,
    /// String used for the `{date}` header/footer template variable.
    /// Defaults to today's date in `YYYY-MM-DD` form. Override to
    /// supply a localised or differently-formatted date (e.g.
    /// `"3 May 2026"`).
    pub date_string: Option<String>,
    /// Caller-supplied template variables, looked up by name in
    /// header/footer and cover-page template strings (`{my_var}`) after
    /// the built-in tokens. Lets a caller surface arbitrary frontmatter
    /// (version, hardware revision, a pre-computed copyright year span,
    /// …) without the renderer knowing those field names. Unknown names
    /// are left as literal `{name}` so typos are visible.
    pub vars: HashMap<String, String>,
}

/// Per-page template context built internally during emission.
pub(crate) struct TemplateContext<'a> {
    pub page: usize,
    pub total: usize,
    pub title: &'a str,
    /// Latest h1 text seen on or before this page.
    pub chapter: &'a str,
    /// Latest h2 text seen on or before this page.
    pub section: &'a str,
    /// Document build date pre-formatted by the renderer (defaults to
    /// the build's date as `YYYY-MM-DD`; can be overridden by the
    /// caller in `RenderContext`).
    pub date: &'a str,
    /// Caller-supplied template variables (from `RenderContext::vars`),
    /// consulted for any `{name}` not in the built-in set.
    pub vars: &'a HashMap<String, String>,
}

impl TemplateContext<'_> {
    /// Substitute `{page}`, `{total}`, `{title}`, `{chapter}`,
    /// `{section}`, `{date}` into a template string. Unknown `{vars}`
    /// are left as-is so authors can spot typos in their templates.
    pub fn substitute(&self, template: &str) -> String {
        let mut out = String::with_capacity(template.len());
        let mut chars = template.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '{' {
                out.push(c);
                continue;
            }
            let mut name = String::new();
            let mut closed = false;
            for next in chars.by_ref() {
                if next == '}' {
                    closed = true;
                    break;
                }
                name.push(next);
            }
            if !closed {
                out.push('{');
                out.push_str(&name);
                continue;
            }
            match name.as_str() {
                "page" => out.push_str(&self.page.to_string()),
                "total" => out.push_str(&self.total.to_string()),
                "title" => out.push_str(self.title),
                "chapter" => out.push_str(self.chapter),
                "section" => out.push_str(self.section),
                "date" => out.push_str(self.date),
                other => match self.vars.get(other) {
                    Some(v) => out.push_str(v),
                    None => {
                        // Unknown — leave literal so typos are visible.
                        out.push('{');
                        out.push_str(&name);
                        out.push('}');
                    }
                },
            }
        }
        out
    }
}

/// Render a transformed Markdoc tree to PDF bytes using the given style.
/// Media references resolve through the null resolver (placeholder text);
/// for documents that contain images, use [`render_pdf_with_assets`].
#[allow(unused_mut)] // krilla's Surface lint misfires; binding kept mut for clarity
pub fn render_pdf(root: &RenderableTreeNode, style: &Style) -> Vec<u8> {
    render_pdf_with(
        root,
        style,
        block::null_resolver(),
        &RenderContext::default(),
    )
}

/// Render with a caller-supplied asset resolver. Equivalent to
/// `render_pdf_with` with a default `RenderContext`.
#[allow(unused_mut)]
pub fn render_pdf_with_assets(
    root: &RenderableTreeNode,
    style: &Style,
    assets: &dyn AssetResolver,
) -> Vec<u8> {
    render_pdf_with(root, style, assets, &RenderContext::default())
}

/// Full-control entry: caller provides assets resolver and `RenderContext`
/// (used for header/footer template substitution like `{title}`).
#[allow(unused_mut)]
pub fn render_pdf_with(
    root: &RenderableTreeNode,
    style: &Style,
    assets: &dyn AssetResolver,
    ctx: &RenderContext,
) -> Vec<u8> {
    let mut font_cx = FontContext::default();
    let mut layout_cx = LayoutContext::new();
    let mut font_cache: HashMap<u64, Font> = HashMap::new();

    // Custom fonts: load every configured .ttf/.otf path into the
    // font collection up-front so all subsequent layout passes see
    // them. Failures are silent — fontique already logs.
    if !style.font_paths.is_empty() {
        font_cx
            .collection
            .load_fonts_from_paths(style.font_paths.iter().map(std::path::Path::new));
    }
    // Resolve the effective body family list once and leak into a
    // 'static slice so layout sites can reference it cheaply.
    let body_families: &'static [&'static str] = if style.body_font_families.is_empty() {
        text::default_families()
    } else {
        let leaked: Vec<&'static str> = style
            .body_font_families
            .iter()
            .map(|s| Box::leak(s.clone().into_boxed_str()) as &'static str)
            .collect();
        Box::leak(leaked.into_boxed_slice())
    };
    let hyphenator = hyphen::WordHyphenator::from_style(&style.hyphenation);

    // 1. Layout.
    let crossref_labels = crossref_labels::build_crossref_labels(root, &style.heading_numbering);
    let (blocks, footnotes) = {
        let mut lctx = LayoutCtx {
            style,
            font_cx: &mut font_cx,
            layout_cx: &mut layout_cx,
            assets,
            next_heading: 0,
            next_figure: 0,
            next_table: 0,
            pending_table_caption: None,
            pending_figure_caption: None,
            footnotes: Vec::new(),
            body_families,
            hyphenator: hyphenator.as_ref(),
            heading_counters: [0; 6],
            list_depth: 0,
            cell_content_align: None,
            crossref_labels: &crossref_labels,
        };
        let blocks = block::layout_document(root, &mut lctx);
        (blocks, lctx.footnotes)
    };

    // Prepend a synthesised cover page when the style enables it.
    // The coverpage builder pulls everything from RenderContext
    // (title / description / authors / date) plus the configured logo,
    // and ends with a PageBreak so body content starts on page 2.
    let date_for_coverpage = ctx
        .date_string
        .clone()
        .unwrap_or_else(crate::dates::today_yyyy_mm_dd);
    let mut blocks = blocks;
    let mut cover_page_count = 0usize;
    if style.coverpage.enabled {
        let mut fp = coverpage::build_coverpage_blocks(
            style,
            ctx,
            body_families,
            assets,
            &mut font_cx,
            &mut layout_cx,
            &date_for_coverpage,
        );
        cover_page_count = fp
            .iter()
            .filter(|b| matches!(b.draw, block::BlockDraw::PageBreak))
            .count();
        fp.append(&mut blocks);
        blocks = fp;
    }

    // 2. Paginate. Reserve vertical space for any header/footer.
    let header_reserved = style
        .page_decoration
        .header
        .as_ref()
        .map(|h| h.reserved_height())
        .unwrap_or(0.0);
    let footer_reserved = style
        .page_decoration
        .footer
        .as_ref()
        .map(|f| f.reserved_height())
        .unwrap_or(0.0);
    let banner_band_top = style.margin_y * 0.42;
    let banner_reserved = style
        .page_decoration
        .banner
        .as_ref()
        .map(|b| (banner_band_top + b.height + 12.0 - style.margin_y).max(0.0))
        .unwrap_or(0.0);
    let page_budget = style.page_height
        - 2.0 * style.margin_y
        - header_reserved
        - footer_reserved
        - banner_reserved;
    let body_start_y = style.margin_y + header_reserved + banner_reserved;
    let inner_x = style.margin_x;
    let inner_w = style.page_width - 2.0 * style.margin_x;

    let cover_first_page = style.coverpage.enabled
        && (style.coverpage.margin_x.is_some() || style.coverpage.margin_y.is_some());
    let cover_margin_y = style.coverpage.margin_y.unwrap_or(style.margin_y);
    let cover_hf = if style.page_decoration.skip_first_page {
        0.0
    } else {
        header_reserved + footer_reserved + banner_reserved
    };
    let first_page_budget = if cover_first_page {
        style.page_height - 2.0 * cover_margin_y - cover_hf
    } else {
        page_budget
    };
    let first_page_start_y = if cover_first_page {
        let hdr = if style.page_decoration.skip_first_page {
            0.0
        } else {
            header_reserved + banner_reserved
        };
        cover_margin_y + hdr
    } else {
        body_start_y
    };

    let column_layout = if style.page_layout.columns > 1 {
        Some(emit::PageColumnLayout {
            columns: style.page_layout.columns,
            column_width: (inner_w - style.page_layout.gap * (style.page_layout.columns as f32 - 1.0))
                / style.page_layout.columns as f32,
            gap: style.page_layout.gap,
        })
    } else {
        None
    };

    // Footnote-aware pagination: pool height is recomputed every time
    // a block carrying footnote calls is added, so the body budget
    // shrinks dynamically as footnotes accumulate.
    let body_with_pools: Vec<(Vec<block::Block>, Vec<block::Block>)> = {
        let footnotes = &footnotes;
        paginate::paginate_with_footnotes(
            blocks,
            page_budget,
            first_page_budget,
            style.page_layout.columns,
            |numbers| {
            if numbers.is_empty() {
                return (0.0, Vec::new());
            }
            let mut entries: Vec<(u32, String)> = numbers
                .iter()
                .filter_map(|n| {
                    footnotes
                        .get((*n as usize).saturating_sub(1))
                        .filter(|b| !b.is_empty())
                        .map(|b| (*n, b.clone()))
                })
                .collect();
            entries.sort_by_key(|(n, _)| *n);
            entries.dedup_by_key(|(n, _)| *n);
            let pool = block::build_footnote_pool_blocks(
                &entries,
                style,
                body_families,
                &mut font_cx,
                &mut layout_cx,
                inner_x,
                inner_w,
            );
            let h = block::pool_height(&pool);
            (h, pool)
        })
    };

    // ── Optional: synthesise & insert ToC / List of Figures / List of Tables ─
    // Where the start-positioned front matter splits from the body: after
    // the cover by default, or — when the document carries a `{% toc /%}`
    // marker — after the page that marker falls on (so any front-matter
    // sections before it, e.g. a copyright page, precede the ToC).
    let split_at = body_with_pools
        .iter()
        .position(|(blocks, _)| {
            blocks
                .iter()
                .any(|b| b.anchor_id.as_deref() == Some(block::TOC_MARKER_ANCHOR))
        })
        .map(|marker_page| marker_page + 1)
        .unwrap_or(cover_page_count);
    let any_section = style.toc.enabled || style.lof.enabled || style.lot.enabled;
    let mut pages: Vec<(Vec<block::Block>, Vec<block::Block>)> = if any_section {
        build_with_front_matter_pools(
            body_with_pools,
            style,
            body_families,
            &mut font_cx,
            &mut layout_cx,
            page_budget,
            split_at,
        )
    } else {
        body_with_pools
    };
    // Duplex padding: if the document ends on an odd page, append one
    // page so the physical sheet count is even. The padding page carries
    // the running header / footer (and watermark) like any other page, but
    // has no body content. `total_pages` is taken AFTER padding so the
    // `{total}` page-of count includes it.
    if needs_even_padding(pages.len(), style.pad_to_even) {
        pages.push((Vec::new(), Vec::new()));
    }
    let total_pages = pages.len();

    // Where the last-page QR stamp lands: the last page carrying body content
    // (never a blank duplex-padding page) — unless `include_padding_page` opts
    // into the physical last page (the back of the last sheet).
    let last_content_idx = pages.iter().rposition(|(b, _)| !b.is_empty());
    let qr_page_idx = match &style.page_decoration.last_page_qr {
        Some(q) if q.include_padding_page => total_pages.checked_sub(1),
        _ => last_content_idx,
    };

    // 3. Emit. Each page collects link annotations and outline points
    //    locally; outline points carry their page index so the final
    //    outline tree can be built once all pages are emitted. We also
    //    track `current_chapter` (latest h1 text) across pages so it
    //    can be substituted into header/footer templates.
    // Pre-pass: walk the paginated blocks just summing y positions to
    // collect every anchor's (page_idx, y). This map is then used during
    // emit to resolve `{% tagref %}` internal links (`href = "#id"`)
    // to PDF GoTo destinations. We can't defer this until after the
    // emit loop because annotations must be added before page.finish().
    let anchor_map = collect_anchor_map(&pages, body_start_y, column_layout);

    let mut document = build_document(style.pdf_export);
    apply_metadata(&mut document, ctx);

    // PDF/UA-1 (and good practice for accessibility tools) requires a
    // structure tree describing the document's logical hierarchy. We
    // collect tag-tree fragments per page and assemble them at the end.
    let tagging_enabled = matches!(style.pdf_export, PdfExportProfile::UA1);
    let mut document_children: Vec<krilla::tagging::Node> = Vec::new();

    let mut all_outline: Vec<(usize, emit::OutlinePoint)> = Vec::new();
    let mut current_chapter: String = String::new();
    let mut current_section: String = String::new();
    let mut media_cache: decoration::MediaCache = HashMap::new();
    // Pre-format the document date once for `{date}` substitution.
    // Caller-supplied string wins; otherwise we stamp today's date.
    let date_str = ctx
        .date_string
        .clone()
        .unwrap_or_else(crate::dates::today_yyyy_mm_dd);

    for (page_idx, (page_blocks, pool_blocks)) in pages.iter().enumerate() {
        let mut page = document
            .start_page_with(PageSettings::from_wh(style.page_width, style.page_height).unwrap());
        let mut links: Vec<emit::DeferredLink> = Vec::new();
        let mut outline_pts: Vec<emit::OutlinePoint> = Vec::new();
        let mut page_tags = emit::TagAccumulator::new(tagging_enabled);
        {
            let mut surface = page.surface();
            // Watermark first so body + decoration paint over it.
            if let Some(wm) = &style.watermark {
                decoration::emit_watermark(
                    &mut surface,
                    style,
                    wm,
                    page_idx,
                    &mut font_cx,
                    &mut layout_cx,
                    &mut font_cache,
                    assets,
                    &mut media_cache,
                    tagging_enabled,
                );
            }
            // The cover page (page 0) may start at its own top margin.
            let page_start_y = if page_idx == 0 {
                first_page_start_y
            } else {
                body_start_y
            };
            emit::emit_blocks(
                &mut surface,
                page_blocks,
                page_start_y,
                &mut font_cache,
                &mut links,
                &mut outline_pts,
                &mut page_tags,
                column_layout,
            );

            // Footnote pool: floats at the bottom of the printable
            // area regardless of how much body text the page carries.
            // Pool blocks contain a leading gap + separator rule plus
            // one paragraph per footnote entry.
            if !pool_blocks.is_empty() {
                let pool_h = block::pool_height(pool_blocks);
                let pool_top_y = body_start_y + page_budget - pool_h;
                emit::emit_blocks(
                    &mut surface,
                    pool_blocks,
                    pool_top_y,
                    &mut font_cache,
                    &mut links,
                    &mut outline_pts,
                    &mut page_tags,
                    None,
                );
            }

            // Update running chapter / section for this page (h1 → chapter,
            // h2 → section). Whichever heading appears LAST on the page
            // wins for that page's header/footer.
            for pt in &outline_pts {
                match pt.level {
                    1 => current_chapter = pt.text.clone(),
                    2 => current_section = pt.text.clone(),
                    _ => {}
                }
            }

            let tctx = TemplateContext {
                page: page_idx + 1,
                total: total_pages,
                title: &ctx.title,
                chapter: &current_chapter,
                section: &current_section,
                date: &date_str,
                vars: &ctx.vars,
            };
            // Page-level decoration: skip on first page if configured. The
            // duplex-padding page is decorated like any other (header /
            // footer); it just has no body.
            let draw_decoration = !(style.page_decoration.skip_first_page && page_idx == 0);
            if draw_decoration {
                if let Some(header) = &style.page_decoration.header {
                    decoration::emit_header(
                        &mut surface,
                        style,
                        header,
                        &tctx,
                        &mut font_cx,
                        &mut layout_cx,
                        &mut font_cache,
                        assets,
                        &mut media_cache,
                        tagging_enabled,
                    );
                }
                if let Some(footer) = &style.page_decoration.footer {
                    decoration::emit_footer(
                        &mut surface,
                        style,
                        footer,
                        &tctx,
                        &mut font_cx,
                        &mut layout_cx,
                        &mut font_cache,
                        assets,
                        &mut media_cache,
                        tagging_enabled,
                    );
                }
                if let Some(banner) = &style.page_decoration.banner {
                    decoration::emit_notice_banner(
                        &mut surface,
                        style,
                        banner,
                        banner_band_top,
                        &tctx,
                        &mut font_cx,
                        &mut layout_cx,
                        &mut font_cache,
                        assets,
                        &mut media_cache,
                        tagging_enabled,
                    );
                }
            }
            // Last-page QR stamp — bottom-right corner of the final content
            // page, drawn regardless of the header/footer skip logic.
            if Some(page_idx) == qr_page_idx
                && let Some(qr) = &style.page_decoration.last_page_qr
            {
                decoration::emit_last_page_qr(
                    &mut surface,
                    style,
                    qr,
                    &tctx,
                    &mut font_cx,
                    &mut layout_cx,
                    &mut font_cache,
                    tagging_enabled,
                );
            }
            surface.finish();
        }
        for link in links {
            // Build the per-line rect list. Skip the link if every
            // rect is malformed (e.g. zero-width sliver from a layout
            // edge case).
            let quads: Vec<krilla::geom::Quadrilateral> = link
                .rects
                .iter()
                .filter_map(|(x, y, w, h)| Rect::from_xywh(*x, *y, *w, *h))
                .map(krilla::geom::Quadrilateral::from)
                .collect();
            if quads.is_empty() {
                continue;
            }
            // PDF/UA requires alt text on every annotation. Fall back
            // to the href so the link is still self-describing.
            let alt = link.alt.clone().or_else(|| Some(link.href.clone()));
            let target = if let Some(id) = link.href.strip_prefix('#') {
                match anchor_map.get(id) {
                    Some((dest_page, dest_y)) => {
                        Target::Action(Action::Goto(krilla::destination::Destination::Xyz(
                            XyzDestination::new(*dest_page, Point::from_xy(0.0, *dest_y)),
                        )))
                    }
                    None => Target::Action(Action::Link(LinkAction::new(link.href.clone()))),
                }
            } else {
                Target::Action(Action::Link(LinkAction::new(link.href.clone())))
            };
            // Single-line links use a plain rect; wrapped links use
            // quad_points so the annotation hot region follows the
            // text shape rather than enclosing the gap between lines.
            let link_annot = if quads.len() == 1 {
                LinkAnnotation::new(
                    Rect::from_xywh(
                        link.rects[0].0,
                        link.rects[0].1,
                        link.rects[0].2,
                        link.rects[0].3,
                    )
                    .unwrap(),
                    target,
                )
            } else {
                LinkAnnotation::new_with_quad_points(quads, target)
            };
            let annot = Annotation::new_link(link_annot, alt);
            // For tagged docs, the annotation's structure-tree role is a
            // single `Link` tag group whose children are every text Span
            // identifier produced for this link (one per wrapped line)
            // followed by the annotation identifier returned by
            // add_tagged_annotation. Each identifier has exactly one
            // parent in the tree, so the link's text identifiers are
            // intentionally absent from the surrounding `P` group.
            if tagging_enabled {
                let annot_id = page.add_tagged_annotation(annot);
                let mut link_group = TagGroup::new(TagKind::Link(Tag::<kind::Link>::Link));
                for text_id in link.text_segment_ids {
                    link_group.push(text_id);
                }
                link_group.push(annot_id);
                page_tags.nodes.push(link_group.into());
            } else {
                page.add_annotation(annot);
            }
        }
        for pt in outline_pts {
            all_outline.push((page_idx, pt));
        }
        if tagging_enabled {
            // Group all of this page's content tags into one Section,
            // then push that into the document children.
            let mut section = TagGroup::new(TagKind::Section(Tag::<kind::Section>::Section));
            for n in page_tags.nodes {
                section.push(n);
            }
            document_children.push(section.into());
        }
        page.finish();
    }

    // 4. Build the outline tree from collected (page_idx, level, text, y)
    //    tuples and attach to the document.
    let outline = build_outline(&all_outline);
    document.set_outline(outline);

    // 5. Build the tag tree (PDF/UA-1) from per-page tag fragments.
    if tagging_enabled {
        let mut tree = TagTree::new();
        if let Some(lang) = ctx.language.clone() {
            tree = tree.with_lang(Some(lang));
        }
        for n in document_children {
            tree.push(n);
        }
        document.set_tag_tree(tree);
    }

    // Edge case: empty document → ensure at least one (blank) page exists
    // so viewers don't choke on a zero-page PDF.
    if document_has_no_pages(&document) {
        let mut page = document
            .start_page_with(PageSettings::from_wh(style.page_width, style.page_height).unwrap());
        let mut surface = page.surface();
        surface.finish();
        page.finish();
    }

    document.finish().unwrap()
}

fn document_has_no_pages(_doc: &Document) -> bool {
    // krilla doesn't expose a page count; track it externally if needed.
    // For now, assume non-empty input always produces at least one page.
    false
}

/// Walk paginated blocks (without rendering) to compute every anchor's
/// page index and y position. Used to build the cross-reference map
/// before per-page emit so internal `{% tagref %}` links can resolve
/// to PDF GoTo destinations.
fn collect_anchor_map(
    pages: &[(Vec<block::Block>, Vec<block::Block>)],
    body_start_y: f32,
    columns: Option<emit::PageColumnLayout>,
) -> std::collections::HashMap<String, (usize, f32)> {
    let mut map = std::collections::HashMap::new();
    for (page_idx, (page_blocks, _pool)) in pages.iter().enumerate() {
        walk_anchors(page_blocks, page_idx, body_start_y, columns, &mut map);
    }
    map
}

fn walk_anchors(
    blocks: &[block::Block],
    page_idx: usize,
    start_y: f32,
    columns: Option<emit::PageColumnLayout>,
    map: &mut std::collections::HashMap<String, (usize, f32)>,
) {
    if columns.map(|c| c.columns).unwrap_or(1) <= 1 {
        walk_anchors_single(blocks, page_idx, start_y, map);
        return;
    }
    let layout = columns.unwrap();
    let n = layout.columns.max(1) as usize;
    let mut col_y = vec![start_y; n];
    for block in blocks {
        let col_idx = block.page_column.min(layout.columns.saturating_sub(1)) as usize;
        let y = if block.column_span {
            col_y.iter().copied().fold(start_y, f32::max)
        } else {
            col_y[col_idx]
        };
        record_block_anchors(block, page_idx, y, map);
        let advance = block.height + block.space_after;
        if block.column_span {
            let new_y = y + advance;
            for cy in &mut col_y {
                *cy = new_y;
            }
        } else {
            col_y[col_idx] = y + advance;
        }
    }
}

fn walk_anchors_single(
    blocks: &[block::Block],
    page_idx: usize,
    start_y: f32,
    map: &mut std::collections::HashMap<String, (usize, f32)>,
) {
    let mut y = start_y;
    for block in blocks {
        record_block_anchors(block, page_idx, y, map);
        y += block.height + block.space_after;
    }
}

fn record_block_anchors(
    block: &block::Block,
    page_idx: usize,
    y: f32,
    map: &mut std::collections::HashMap<String, (usize, f32)>,
) {
    if let Some(id) = &block.anchor_id {
        map.entry(id.clone()).or_insert((page_idx, y));
    }
    match &block.draw {
        block::BlockDraw::Text(slice) if !slice.mid_anchors.is_empty() => {
            for anchor in slice.mid_anchors.iter() {
                if let Some(line_y_offset) = mid_anchor_y_in_slice(slice, anchor.byte_offset) {
                    map.entry(anchor.id.clone())
                        .or_insert((page_idx, y + line_y_offset));
                }
            }
        }
        block::BlockDraw::BoxedGroup {
            children, padding, ..
        } => {
            walk_anchors_single(children, page_idx, y + *padding, map);
        }
        block::BlockDraw::ListItem { body, .. } => {
            walk_anchors_single(body, page_idx, y, map);
        }
        block::BlockDraw::Table {
            rows,
            cell_padding,
            border_thickness,
            ..
        } => {
            let mut row_top = y + *border_thickness;
            for row in rows {
                for cell in &row.cells {
                    walk_anchors_single(&cell.blocks, page_idx, row_top + *cell_padding, map);
                }
                row_top += row.height + *border_thickness;
            }
        }
        block::BlockDraw::Float { image, wrap } => {
            walk_anchors_single(std::slice::from_ref(image.as_ref()), page_idx, y, map);
            walk_anchors_single(wrap, page_idx, y, map);
        }
        block::BlockDraw::FloatRegion { text, floats } => {
            for anchor in text.mid_anchors.iter() {
                if let Some(dy) = mid_anchor_y_in_slice(text, anchor.byte_offset) {
                    map.entry(anchor.id.clone())
                        .or_insert((page_idx, y + dy));
                }
            }
            for fl in floats {
                if let Some(id) = &fl.image.anchor_id {
                    map.entry(id.clone())
                        .or_insert((page_idx, y + fl.y_offset));
                }
            }
        }
        _ => {}
    }
}

/// Find the y offset (relative to a slice's drawn top) where the line
/// containing `byte_offset` begins. Returns `None` if the byte falls
/// outside this slice's `line_range` (the anchor lives in another
/// slice from a split paragraph).
fn mid_anchor_y_in_slice(slice: &block::TextSlice, byte_offset: usize) -> Option<f32> {
    let line_idx = find_line_for_byte(&slice.layout, byte_offset);
    if line_idx < slice.line_range.start || line_idx >= slice.line_range.end {
        return None;
    }
    let y: f32 = slice.line_heights[slice.line_range.start..line_idx]
        .iter()
        .copied()
        .sum();
    Some(y)
}

fn find_line_for_byte(layout: &parley::Layout<krilla::color::rgb::Color>, byte: usize) -> usize {
    let mut last = 0usize;
    for (i, line) in layout.lines().enumerate() {
        last = i;
        for run in line.runs() {
            for cluster in run.visual_clusters() {
                let r = cluster.text_range();
                if byte >= r.start && byte < r.end {
                    return i;
                }
                if byte == r.end {
                    return i;
                }
            }
        }
    }
    last
}

/// Build a hierarchical PDF outline from flat heading entries.
///
/// Algorithm: walk entries in source order maintaining a stack of
/// (level, in-progress OutlineNode). For each new heading:
///   - pop the stack while top.level >= new.level
///   - if stack non-empty, attach the new node as a child of top
///   - else push as a top-level node when complete
///
/// Because OutlineNode children are immutable once attached, we keep
/// a parallel stack of "open" nodes whose children we still mutate, and
/// flush them to their parent / the outline root when popped.
fn build_outline(points: &[(usize, emit::OutlinePoint)]) -> Outline {
    let mut outline = Outline::new();
    if points.is_empty() {
        return outline;
    }

    // Stack: each entry is (level, partially-built OutlineNode).
    let mut stack: Vec<(u8, OutlineNode)> = Vec::new();

    let flush_into = |stack: &mut Vec<(u8, OutlineNode)>, out: &mut Outline, node: OutlineNode| {
        if let Some(parent) = stack.last_mut() {
            parent.1.push_child(node);
        } else {
            out.push_child(node);
        }
    };

    for (page_idx, pt) in points {
        // Pop deeper-or-equal-level nodes until parent has a strictly
        // smaller level.
        while stack.last().is_some_and(|(lvl, _)| *lvl >= pt.level) {
            let (_, completed) = stack.pop().unwrap();
            flush_into(&mut stack, &mut outline, completed);
        }
        let dest = XyzDestination::new(*page_idx, Point::from_xy(0.0, pt.y));
        stack.push((pt.level, OutlineNode::new(pt.text.clone(), dest)));
    }
    // Flush remaining nodes.
    while let Some((_, completed)) = stack.pop() {
        flush_into(&mut stack, &mut outline, completed);
    }
    outline
}

/// Build any combination of ToC / List of Figures / List of Tables
/// from the paginated body, paginate each section, and concatenate
/// them in canonical order (ToC → LoF → LoT) at start or end.
///
/// Page numbers in the section entries reflect the FINAL PDF page
/// numbers, accounting for the combined start-placement section
/// length. We iterate up to 4 times to converge — single-digit ↔
/// double-digit transitions can in theory shift pagination, but in
/// practice everything stabilises in 1 pass.
/// Pool-aware wrapper around [`build_with_front_matter`]. Body pages
/// carry the pool the paginator built for them; front-matter pages
/// pair with an empty pool (ToC/LoF/LoT never reference footnotes).
fn build_with_front_matter_pools(
    mut body_with_pools: Vec<(Vec<block::Block>, Vec<block::Block>)>,
    style: &Style,
    body_families: &'static [&'static str],
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<krilla::color::rgb::Color>,
    page_budget: f32,
    split_at: usize,
) -> Vec<(Vec<block::Block>, Vec<block::Block>)> {
    let body_pages: Vec<Vec<block::Block>> =
        body_with_pools.iter().map(|(b, _)| b.clone()).collect();
    let (start_pages, _body_through, end_pages) = build_front_matter_split(
        body_pages,
        style,
        body_families,
        font_cx,
        layout_cx,
        page_budget,
        split_at,
    );

    // The leading `split_at` body pages — the cover plus any front matter
    // before a `{% toc /%}` marker — come first; start-positioned ToC /
    // LoF / LoT slot in *after* them, and the rest of the body follows.
    let k = split_at.min(body_with_pools.len());
    let rest_body = body_with_pools.split_off(k);
    let leading_pages = body_with_pools; // the leading `k` (body, pool) pages

    let mut out = leading_pages;
    out.extend(start_pages.into_iter().map(|p| (p, Vec::new())));
    out.extend(rest_body);
    out.extend(end_pages.into_iter().map(|p| (p, Vec::new())));
    out
}

/// Variant of [`build_with_front_matter`] that returns the
/// `(start_pages, body_pages, end_pages)` split so the caller can
/// re-attach footnote pools to body pages without re-locating them in
/// the concatenated output.
/// Three sets of "front matter" pages — table of contents, list of
/// figures, list of tables — extracted from the body pages and laid out
/// independently. Each outer `Vec` is the pages, inner is the blocks on
/// that page.
type FrontMatterPages = (
    Vec<Vec<block::Block>>,
    Vec<Vec<block::Block>>,
    Vec<Vec<block::Block>>,
);

fn build_front_matter_split(
    body_pages: Vec<Vec<block::Block>>,
    style: &Style,
    body_families: &'static [&'static str],
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<krilla::color::rgb::Color>,
    page_budget: f32,
    split_at: usize,
) -> FrontMatterPages {
    let headings = harvest_heading_entries(&body_pages);
    let figures = harvest_figure_entries(&body_pages);
    let tables = harvest_table_entries(&body_pages);

    let mut start_offset: usize = if any_start_section_enabled(style) {
        1
    } else {
        0
    };

    let mut start_pages: Vec<Vec<block::Block>> = Vec::new();
    let mut end_pages: Vec<Vec<block::Block>> = Vec::new();

    for _ in 0..4 {
        start_pages.clear();
        end_pages.clear();

        if style.toc.enabled {
            let entries: Vec<block::TocEntry> = headings
                .iter()
                .map(|h| block::TocEntry {
                    level: h.level,
                    text: h.text.clone(),
                    target_anchor_id: h.anchor_id.clone(),
                    page_number: page_number_for(
                        h.body_page_idx,
                        style.toc.position,
                        start_offset,
                        split_at,
                    ),
                })
                .collect();
            let blocks =
                block::build_toc_blocks(&entries, style, body_families, font_cx, layout_cx);
            let pages = paginate::paginate(blocks, page_budget);
            push_section(&mut start_pages, &mut end_pages, style.toc.position, pages);
        }
        if style.lof.enabled {
            let entries: Vec<block::TocEntry> = figures
                .iter()
                .enumerate()
                .map(|(i, f)| block::TocEntry {
                    level: 1,
                    text: figure_caption(i, f, style),
                    target_anchor_id: f.anchor_id.clone(),
                    page_number: page_number_for(
                        f.body_page_idx,
                        style.lof.position,
                        start_offset,
                        split_at,
                    ),
                })
                .collect();
            let blocks = block::build_list_section_blocks(
                style.lof.resolved_title("List of Figures"),
                style.lof.title_font_size,
                style.lof.entry_font_size,
                style.lof.entry_space_after,
                &entries,
                style,
                body_families,
                font_cx,
                layout_cx,
            );
            let pages = paginate::paginate(blocks, page_budget);
            push_section(&mut start_pages, &mut end_pages, style.lof.position, pages);
        }
        if style.lot.enabled {
            let entries: Vec<block::TocEntry> = tables
                .iter()
                .enumerate()
                .map(|(i, t)| block::TocEntry {
                    level: 1,
                    text: table_caption(i, t, style),
                    target_anchor_id: t.anchor_id.clone(),
                    page_number: page_number_for(
                        t.body_page_idx,
                        style.lot.position,
                        start_offset,
                        split_at,
                    ),
                })
                .collect();
            let blocks = block::build_list_section_blocks(
                style.lot.resolved_title("List of Tables"),
                style.lot.title_font_size,
                style.lot.entry_font_size,
                style.lot.entry_space_after,
                &entries,
                style,
                body_families,
                font_cx,
                layout_cx,
            );
            let pages = paginate::paginate(blocks, page_budget);
            push_section(&mut start_pages, &mut end_pages, style.lot.position, pages);
        }
        let new_offset = start_pages.len();
        if new_offset == start_offset {
            break;
        }
        start_offset = new_offset;
    }
    (start_pages, body_pages, end_pages)
}

fn any_start_section_enabled(style: &Style) -> bool {
    (style.toc.enabled && style.toc.position == TocPosition::Start)
        || (style.lof.enabled && style.lof.position == TocPosition::Start)
        || (style.lot.enabled && style.lot.position == TocPosition::Start)
}

/// Final 1-based page number for a heading on body page `body_idx`.
///
/// For a start-positioned ToC, the `start_offset` ToC pages sit at
/// `split_at` (after the leading cover / front-matter pages). A heading
/// in those leading pages (`body_idx < split_at`) keeps its natural
/// number; a heading after the split is pushed down by `start_offset`.
fn page_number_for(
    body_idx: usize,
    position: TocPosition,
    start_offset: usize,
    split_at: usize,
) -> usize {
    match position {
        TocPosition::End => body_idx + 1,
        TocPosition::Start if body_idx < split_at => body_idx + 1,
        TocPosition::Start => body_idx + start_offset + 1,
    }
}

fn push_section(
    start: &mut Vec<Vec<block::Block>>,
    end: &mut Vec<Vec<block::Block>>,
    position: TocPosition,
    pages: Vec<Vec<block::Block>>,
) {
    match position {
        TocPosition::Start => start.extend(pages),
        TocPosition::End => end.extend(pages),
    }
}

fn figure_caption(index: usize, f: &FigureEntry, style: &Style) -> String {
    let n = index + 1;
    let prefix = &style.figure_caption_prefix;
    let sep = &style.caption_separator;
    match &f.caption {
        Some(c) if !c.trim().is_empty() => format!("{prefix} {n}{sep} {}", c.trim()),
        _ => format!("{prefix} {n}"),
    }
}

fn table_caption(index: usize, t: &TableEntry, style: &Style) -> String {
    let n = index + 1;
    let prefix = &style.table_caption_prefix;
    let sep = &style.caption_separator;
    match &t.caption {
        Some(c) if !c.trim().is_empty() => format!("{prefix} {n}{sep} {}", c.trim()),
        _ => format!("{prefix} {n}"),
    }
}

/// One heading harvested from the paginated body: text, level, anchor
/// id (for linking), and the body-relative page index.
struct HeadingEntry {
    level: u8,
    text: String,
    anchor_id: String,
    body_page_idx: usize,
}

struct FigureEntry {
    caption: Option<String>,
    anchor_id: String,
    body_page_idx: usize,
}

struct TableEntry {
    anchor_id: String,
    body_page_idx: usize,
    caption: Option<String>,
}

fn harvest_figure_entries(pages: &[Vec<block::Block>]) -> Vec<FigureEntry> {
    let mut out = Vec::new();
    for (page_idx, page) in pages.iter().enumerate() {
        walk_for_figures(page, page_idx, &mut out);
    }
    out
}

fn walk_for_figures(blocks: &[block::Block], page_idx: usize, out: &mut Vec<FigureEntry>) {
    for block in blocks {
        let (caption, is_figure) = match &block.draw {
            block::BlockDraw::Image { caption, .. } => (caption.clone(), true),
            block::BlockDraw::Svg { caption, .. } => (caption.clone(), true),
            _ => (None, false),
        };
        if is_figure && let Some(id) = &block.anchor_id {
            out.push(FigureEntry {
                caption,
                anchor_id: id.clone(),
                body_page_idx: page_idx,
            });
        }
        // Recurse — figures can sit inside callouts, list items, table cells.
        match &block.draw {
            block::BlockDraw::BoxedGroup { children, .. } => {
                walk_for_figures(children, page_idx, out);
            }
            block::BlockDraw::ListItem { body, .. } => {
                walk_for_figures(body, page_idx, out);
            }
            block::BlockDraw::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        walk_for_figures(&cell.blocks, page_idx, out);
                    }
                }
            }
            block::BlockDraw::Float { image, wrap } => {
                // The floated image is a figure; the wrap may hold more.
                walk_for_figures(std::slice::from_ref(image.as_ref()), page_idx, out);
                walk_for_figures(wrap, page_idx, out);
            }
            block::BlockDraw::FloatRegion { floats, .. } => {
                for fl in floats {
                    walk_for_figures(std::slice::from_ref(fl.image.as_ref()), page_idx, out);
                }
            }
            _ => {}
        }
    }
}

fn harvest_table_entries(pages: &[Vec<block::Block>]) -> Vec<TableEntry> {
    let mut out = Vec::new();
    for (page_idx, page) in pages.iter().enumerate() {
        walk_for_tables(page, page_idx, &mut out);
    }
    out
}

fn walk_for_tables(blocks: &[block::Block], page_idx: usize, out: &mut Vec<TableEntry>) {
    for block in blocks {
        if let block::BlockDraw::Table { caption, .. } = &block.draw
            && let Some(id) = &block.anchor_id
        {
            out.push(TableEntry {
                anchor_id: id.clone(),
                body_page_idx: page_idx,
                caption: caption.clone(),
            });
        }
        match &block.draw {
            block::BlockDraw::BoxedGroup { children, .. } => {
                walk_for_tables(children, page_idx, out);
            }
            block::BlockDraw::ListItem { body, .. } => {
                walk_for_tables(body, page_idx, out);
            }
            block::BlockDraw::Float { wrap, .. } => {
                walk_for_tables(wrap, page_idx, out);
            }
            _ => {}
        }
    }
}

/// Walk paginated body blocks to harvest every heading entry with its
/// page index. Used to seed the TOC.
fn harvest_heading_entries(pages: &[Vec<block::Block>]) -> Vec<HeadingEntry> {
    let mut out = Vec::new();
    for (page_idx, page) in pages.iter().enumerate() {
        walk_for_headings(page, page_idx, &mut out);
    }
    out
}

fn walk_for_headings(blocks: &[block::Block], page_idx: usize, out: &mut Vec<HeadingEntry>) {
    for block in blocks {
        if let (Some(entry), Some(id)) = (&block.outline, &block.anchor_id) {
            out.push(HeadingEntry {
                level: entry.level,
                text: entry.text.clone(),
                anchor_id: id.clone(),
                body_page_idx: page_idx,
            });
        }
        match &block.draw {
            block::BlockDraw::BoxedGroup { children, .. } => {
                walk_for_headings(children, page_idx, out);
            }
            block::BlockDraw::ListItem { body, .. } => {
                walk_for_headings(body, page_idx, out);
            }
            block::BlockDraw::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        walk_for_headings(&cell.blocks, page_idx, out);
                    }
                }
            }
            block::BlockDraw::Float { wrap, .. } => {
                walk_for_headings(wrap, page_idx, out);
            }
            _ => {}
        }
    }
}

/// Construct a krilla `Document` configured for the requested PDF
/// export profile (PDF/A variant, etc.). Default is unvalidated.
fn build_document(profile: PdfExportProfile) -> Document {
    // krilla 0.8 splits validation into independent archival (PDF/A) and
    // accessibility (PDF/UA) families, built via `ConfigurationBuilder`.
    let builder = match profile {
        PdfExportProfile::None => return Document::new(),
        PdfExportProfile::A1B => {
            ConfigurationBuilder::new().with_archival_validator(Archival::A1_B)
        }
        PdfExportProfile::A2B => {
            ConfigurationBuilder::new().with_archival_validator(Archival::A2_B)
        }
        PdfExportProfile::A3B => {
            ConfigurationBuilder::new().with_archival_validator(Archival::A3_B)
        }
        PdfExportProfile::A4 => ConfigurationBuilder::new().with_archival_validator(Archival::A4),
        PdfExportProfile::UA1 => {
            ConfigurationBuilder::new().with_accessibility_validator(Accessibility::UA1)
        }
    };
    // Each profile sets a single validator with a valid PDF-version range,
    // so `finish` can't fail here.
    let configuration = builder
        .finish()
        .expect("single validator always yields a valid configuration");
    let settings = SerializeSettings {
        configuration,
        ..Default::default()
    };
    Document::new_with(settings)
}

/// Apply caller-supplied document metadata. Required for PDF/A
/// compliance (title + language + creation date); harmless for plain export.
fn apply_metadata(document: &mut Document, ctx: &RenderContext) {
    let mut meta = Metadata::new();
    let creation = ctx.creation_date.unwrap_or_else(crate::dates::now);
    meta = meta.creation_date(creation);
    if !ctx.title.is_empty() {
        meta = meta.title(ctx.title.clone());
    }
    if let Some(lang) = ctx.language.clone()
        && !lang.is_empty()
    {
        meta = meta.language(lang);
    }
    if let Some(desc) = ctx.description.clone()
        && !desc.is_empty()
    {
        meta = meta.description(desc);
    }
    if !ctx.authors.is_empty() {
        meta = meta.authors(ctx.authors.clone());
    }
    if let Some(creator) = ctx.creator.clone()
        && !creator.is_empty()
    {
        meta = meta.creator(creator);
    }
    if let Some(producer) = ctx.producer.clone()
        && !producer.is_empty()
    {
        meta = meta.producer(producer);
    }
    document.set_metadata(meta);
}

/// Whether a trailing page must be appended so the physical page count is
/// even. Duplex (double-sided) printing wants each document to begin on the
/// front of a fresh sheet, which requires an even total. `content_pages` is
/// the count before padding; an empty document (0 pages) is already even and
/// is left untouched.
fn needs_even_padding(content_pages: usize, pad_to_even: bool) -> bool {
    pad_to_even && content_pages % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::needs_even_padding;

    #[test]
    fn even_padding_only_pads_odd_counts_when_enabled() {
        // Enabled: an odd page count gains a trailing page; even is untouched.
        assert!(needs_even_padding(1, true));
        assert!(needs_even_padding(3, true));
        assert!(!needs_even_padding(2, true));
        assert!(!needs_even_padding(6, true));
        // An empty document (0 pages) is already even.
        assert!(!needs_even_padding(0, true));
        // Disabled: never pads, regardless of parity.
        assert!(!needs_even_padding(1, false));
        assert!(!needs_even_padding(2, false));
    }
}
