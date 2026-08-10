//! `Block`: the unit of pagination. The layout pass walks a
//! `RenderableTreeNode` tree and produces a flat sequence of these,
//! each with its computed height and pre-laid-out content. Pagination
//! and emission then operate on `Block`s without re-touching the AST.

use std::collections::HashMap;
use std::sync::Arc;

use krilla::color::rgb;
use krilla::image::Image as KrillaImage;
use markdoc::types::{RenderableTreeNode, Scalar, Tag};
use parley::{FontContext, Layout, LayoutContext};
use usvg::Tree as SvgTree;

use crate::assets::{AssetResolver, MediaFormat, NullAssetResolver, sniff_format};

use super::inline::{InlineProp, InlineRange, Inlines, LinkRange, MidAnchor, collect_inlines};
use super::paginate::paginate as paginate_blocks;
use super::style::MarkerSequence;
use super::style::Style;
use super::style::TableColumnSizing;
use super::text::{
    FloatSpec, TextStyle, build_layout, build_layout_aligned, build_layout_float,
    build_layout_float_anchored, measure_first_line_width, monospace_families,
};

/// One unit of paginatable content.
#[derive(Clone)]
pub struct Block {
    /// The content's drawn height (does not include `space_after`).
    pub height: f32,
    /// Vertical gap to leave after this block.
    pub space_after: f32,
    pub draw: BlockDraw,
    /// If this block represents a heading, an entry to add to the
    /// document outline at emit time. The outline entry's destination
    /// points to the page+y where this block lands after pagination.
    pub outline: Option<OutlineEntry>,
    /// Optional anchor id for `{% tag id="X" %}` declarations. Recorded
    /// at emit time as `(id, page_idx, y)` so `{% tagref %}` internal
    /// links can resolve to a PDF destination.
    pub anchor_id: Option<String>,
    /// PDF/UA semantic role override. When set, the emit pass wraps
    /// this block's tagged content in the requested structure-tree
    /// tag instead of the default `P`. Used by the footnote pool to
    /// emit each entry as `Note`.
    pub tag_role: Option<TagRole>,
    /// Page column index when `page_layout.columns > 1` (0 = left).
    pub page_column: u8,
    /// When true the block spans every column on the page (e.g. H1, tables).
    pub column_span: bool,
}

/// Override for the structure-tree tag emit assigns to a Text block.
/// Defaults (when `None`) are: `Hn` for headings, `P` for everything
/// else. Add variants here as PDF/UA wants more semantic flavours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagRole {
    /// `Note` — used for footnote/endnote bodies.
    Note,
}

/// Heading metadata captured at layout time so the emit pass can
/// place an outline entry at the resolved page+y.
#[derive(Debug, Clone)]
pub struct OutlineEntry {
    pub level: u8,
    pub text: String,
}

/// Outcome of attempting to fit a block's content into a partial page.
///
/// Variants intentionally carry `Block` by value rather than `Box<Block>`:
/// each `SplitOutcome` is constructed, immediately destructured by the
/// paginator, and dropped within the same call frame. Boxing buys nothing
/// for this short-lived flow.
#[allow(clippy::large_enum_variant)]
pub enum SplitOutcome {
    /// Block fits whole into the remaining space (returned unchanged).
    Whole(Block),
    /// Block split at a line boundary: head fits in the remaining space,
    /// tail is the rest.
    Split(Block, Block),
    /// Block can't be split further (or even one line wouldn't fit in
    /// the remaining space). Caller should start a new page.
    NoFit(Block),
}

impl Block {
    /// Footnote numbers whose call mark sits inside this block (and its
    /// descendants). Used by pagination to figure out which footnote
    /// bodies must accompany this block on its destination page.
    pub fn collect_footnote_numbers(&self, out: &mut Vec<u32>) {
        match &self.draw {
            BlockDraw::Text(slice) => out.extend(slice.footnote_numbers()),
            BlockDraw::BoxedGroup { children, .. } => {
                for c in children {
                    c.collect_footnote_numbers(out);
                }
            }
            BlockDraw::ListItem { body, .. } => {
                for c in body {
                    c.collect_footnote_numbers(out);
                }
            }
            BlockDraw::Float { image, wrap } => {
                image.collect_footnote_numbers(out);
                for c in wrap {
                    c.collect_footnote_numbers(out);
                }
            }
            BlockDraw::FloatRegion { text, .. } => out.extend(text.footnote_numbers()),
            // Tables, images, SVG, rules carry no footnote calls in v1.
            _ => {}
        }
    }

    /// True when this block is a heading (carries an outline entry).
    pub fn is_heading(&self) -> bool {
        self.outline.is_some()
    }

    /// True when this is a list item (marker + body).
    pub fn is_list_item(&self) -> bool {
        matches!(self.draw, BlockDraw::ListItem { .. })
    }

    /// Vertical whitespace spacer synthesised by layout (`spacer_block`).
    pub fn is_spacer(&self) -> bool {
        matches!(
            self.draw,
            BlockDraw::Rule {
                width: 0.0,
                thickness: 0.0,
                ..
            }
        )
    }

    /// Image / SVG figure (including captioned ones).
    pub fn is_figure(&self) -> bool {
        matches!(
            self.draw,
            BlockDraw::Image { .. }
                | BlockDraw::Svg { .. }
                | BlockDraw::Float { .. }
                | BlockDraw::FloatRegion { .. }
        )
    }

    /// A table block.
    pub fn is_table(&self) -> bool {
        matches!(self.draw, BlockDraw::Table { .. })
    }

    /// Callout / notice / blockquote / panel — any `BoxedGroup` content
    /// block (as opposed to a transparent grid-row wrapper used only for
    /// pagination atomicity; those still count as real content when they
    /// follow a heading).
    pub fn is_boxed_text_block(&self) -> bool {
        matches!(self.draw, BlockDraw::BoxedGroup { .. })
    }

    /// True when this block alone is enough content after a heading to
    /// allow a page break: list item, figure, table, or a callout /
    /// notice / other boxed text block. (Plain text still needs
    /// ≥3 lines — see `heading_has_enough_followers`.)
    pub fn is_substantial_follower(&self) -> bool {
        self.is_list_item()
            || self.is_figure()
            || self.is_table()
            || self.is_boxed_text_block()
    }

    /// Number of text lines in this block (paragraphs / headings only).
    pub fn text_line_count(&self) -> usize {
        match &self.draw {
            BlockDraw::Text(slice) => slice.line_range.end.saturating_sub(slice.line_range.start),
            _ => 0,
        }
    }

    /// Try to fit at most `remaining` of vertical space. Currently only
    /// `BlockDraw::Text` is splittable; other block kinds return `Whole`
    /// (if they fit) or `NoFit` (if they don't).
    pub fn try_split(self, remaining: f32) -> SplitOutcome {
        let needed = self.height + self.space_after;
        if needed <= remaining {
            return SplitOutcome::Whole(self);
        }
        // Headings never split mid-title — move the whole heading to the
        // next page so we never orphan part of a heading at the bottom.
        if self.is_heading() {
            let Block {
                height,
                space_after,
                draw,
                outline,
                anchor_id,
                tag_role,
                page_column,
                column_span,
            } = self;
            return SplitOutcome::NoFit(Block {
                height,
                space_after,
                draw,
                outline,
                anchor_id,
                tag_role,
                page_column,
                column_span,
            });
        }
        let Block {
            height,
            space_after,
            draw,
            outline,
            anchor_id,
            tag_role,
            page_column,
            column_span,
        } = self;
        match draw {
            BlockDraw::Text(slice) => try_split_text(
                slice,
                space_after,
                remaining,
                page_column,
                column_span,
                tag_role,
                anchor_id,
                outline,
            ),
            BlockDraw::Table {
                x,
                column_widths,
                rows,
                cell_padding,
                header_bg,
                border_color,
                border_thickness,
                border_style,
                edge,
                caption,
            } => try_split_table(
                x,
                column_widths,
                rows,
                cell_padding,
                header_bg,
                border_color,
                border_thickness,
                border_style,
                edge,
                space_after,
                remaining,
                anchor_id,
                caption,
                page_column,
                column_span,
                tag_role,
                outline,
            ),
            // List items stay atomic unless they are taller than the space
            // on a fresh page (`remaining` ≈ full page). Mid-item breaks on
            // a partially filled page are forbidden — return NoFit so the
            // paginator moves the whole item.
            other => SplitOutcome::NoFit(Block {
                height,
                space_after,
                draw: other,
                outline,
                anchor_id,
                tag_role,
                page_column,
                column_span,
            }),
        }
    }
}

/// Minimum lines of a multi-line paragraph that may sit alone at the
/// bottom or top of a page when the paragraph is split across pages.
const TEXT_SPLIT_MIN_LINES: usize = 3;

fn try_split_text(
    slice: TextSlice,
    space_after: f32,
    remaining: f32,
    page_column: u8,
    column_span: bool,
    tag_role: Option<TagRole>,
    anchor_id: Option<String>,
    outline: Option<OutlineEntry>,
) -> SplitOutcome {
    let start = slice.line_range.start;
    let end = slice.line_range.end;
    let total = end.saturating_sub(start);
    let mut acc = 0.0_f32;
    let mut split_at = start;
    for i in start..end {
        let h = slice.line_heights[i];
        if acc + h > remaining {
            break;
        }
        acc += h;
        split_at = i + 1;
    }
    if split_at == start {
        // Not even one line fits.
        return SplitOutcome::NoFit(Block {
            height: slice.height(),
            space_after,
            draw: BlockDraw::Text(slice),
            outline,
            anchor_id,
            tag_role,
            page_column,
            column_span,
        });
    }
    if split_at == end {
        return SplitOutcome::Whole(Block {
            height: slice.height(),
            space_after,
            draw: BlockDraw::Text(slice),
            outline,
            anchor_id,
            tag_role,
            page_column,
            column_span,
        });
    }

    let head_lines = split_at - start;
    let tail_lines = end - split_at;
    // Orphan / widow control: don't leave fewer than TEXT_SPLIT_MIN_LINES
    // on either side of a split when the paragraph is long enough.
    if total >= TEXT_SPLIT_MIN_LINES
        && (head_lines < TEXT_SPLIT_MIN_LINES || tail_lines < TEXT_SPLIT_MIN_LINES)
    {
        return SplitOutcome::NoFit(Block {
            height: slice.height(),
            space_after,
            draw: BlockDraw::Text(slice),
            outline,
            anchor_id,
            tag_role,
            page_column,
            column_span,
        });
    }

    let head_height: f32 = slice.line_heights[start..split_at].iter().sum();
    let tail_height: f32 = slice.line_heights[split_at..end].iter().sum();
    let head = Block {
        height: head_height,
        space_after: 0.0,
        draw: BlockDraw::Text(TextSlice {
            layout: slice.layout.clone(),
            text: slice.text.clone(),
            links: slice.links.clone(),
            mid_anchors: slice.mid_anchors.clone(),
            footnote_calls: slice.footnote_calls.clone(),
            line_heights: slice.line_heights.clone(),
            x: slice.x,
            line_range: start..split_at,
            skip_y: slice.skip_y,
        }),
        outline: outline.clone(),
        anchor_id: anchor_id.clone(),
        tag_role,
        page_column,
        column_span,
    };
    let tail = Block {
        height: tail_height,
        space_after,
        draw: BlockDraw::Text(TextSlice {
            x: slice.x,
            line_range: split_at..end,
            skip_y: slice.skip_y + head_height,
            layout: slice.layout,
            text: slice.text,
            links: slice.links,
            mid_anchors: slice.mid_anchors,
            footnote_calls: slice.footnote_calls,
            line_heights: slice.line_heights,
        }),
        outline,
        anchor_id,
        tag_role: None,
        page_column,
        column_span,
    };
    SplitOutcome::Split(head, tail)
}

#[allow(clippy::too_many_arguments)]
fn try_split_table(
    x: f32,
    column_widths: Vec<f32>,
    rows: Vec<TableRow>,
    cell_padding: f32,
    header_bg: rgb::Color,
    border_color: rgb::Color,
    border_thickness: f32,
    border_style: super::style::TableBorders,
    edge: Option<(rgb::Color, f32)>,
    space_after: f32,
    remaining: f32,
    anchor_id: Option<String>,
    caption: Option<String>,
    page_column: u8,
    column_span: bool,
    tag_role: Option<TagRole>,
    outline: Option<OutlineEntry>,
) -> SplitOutcome {
    // Header rows form a contiguous prefix (`is_header == true`); they
    // repeat on every continuation page.
    let header_count = rows.iter().take_while(|r| r.is_header).count();

    // Header footprint: header rows + their bordering lines (top edge,
    // each header row's bottom edge). When body follows, the same
    // footprint applies; we only add `row.height + border_thickness`
    // per body row.
    let header_height: f32 = rows[..header_count].iter().map(|r| r.height).sum::<f32>()
        + border_thickness * (header_count as f32 + 1.0);

    // Find the largest body-row prefix that fits.
    let mut included_body = 0;
    let mut footprint = header_height;
    for body_row in &rows[header_count..] {
        let increment = body_row.height + border_thickness;
        if footprint + increment > remaining {
            break;
        }
        footprint += increment;
        included_body += 1;
    }

    if included_body == 0 {
        if header_count < rows.len() {
            let row_available = remaining - header_height - border_thickness;
            if row_available > 0.0
                && let Some((head_row, tail_row)) =
                    try_split_first_body_row(&rows[header_count], row_available, cell_padding)
            {
                let mut head_rows: Vec<TableRow> = rows[..header_count].to_vec();
                head_rows.push(head_row);

                let mut tail_rows: Vec<TableRow> = rows[..header_count].to_vec();
                tail_rows.push(tail_row);
                let body_after_first: Vec<TableRow> =
                    rows.into_iter().skip(header_count + 1).collect();
                tail_rows.extend(body_after_first);

                let head_block = make_table_block(
                    x,
                    column_widths.clone(),
                    head_rows,
                    cell_padding,
                    header_bg,
                    border_color,
                    border_thickness,
                    border_style,
                    edge,
                    0.0,
                    anchor_id.clone(),
                    caption.clone(),
                    page_column,
                    column_span,
                    tag_role,
                    outline.clone(),
                );
                let tail_block = make_table_block(
                    x,
                    column_widths,
                    tail_rows,
                    cell_padding,
                    header_bg,
                    border_color,
                    border_thickness,
                    border_style,
                    edge,
                    space_after,
                    None,
                    None,
                    page_column,
                    column_span,
                    None,
                    None,
                );
                return SplitOutcome::Split(head_block, tail_block);
            }
        }
        return SplitOutcome::NoFit(make_table_block(
            x,
            column_widths,
            rows,
            cell_padding,
            header_bg,
            border_color,
            border_thickness,
            border_style,
            edge,
            space_after,
            anchor_id,
            caption,
            page_column,
            column_span,
            tag_role,
            outline,
        ));
    }
    if header_count + included_body == rows.len() {
        return SplitOutcome::Whole(make_table_block(
            x,
            column_widths,
            rows,
            cell_padding,
            header_bg,
            border_color,
            border_thickness,
            border_style,
            edge,
            space_after,
            anchor_id,
            caption,
            page_column,
            column_span,
            tag_role,
            outline,
        ));
    }

    // Split: head = headers + body[..included]; tail = headers (cloned)
    // + body[included..]. Head retains the anchor; tail has no anchor
    // (the table started on the head's page, and the LoF/LoT entry
    // should point there).
    let split_at = header_count + included_body;
    let header_clone: Vec<TableRow> = rows[..header_count].to_vec();
    let mut head_rows = rows;
    let mut tail_rows = head_rows.split_off(split_at);
    tail_rows.splice(0..0, header_clone);

    let head_block = make_table_block(
        x,
        column_widths.clone(),
        head_rows,
        cell_padding,
        header_bg,
        border_color,
        border_thickness,
        border_style,
        edge,
        0.0,
        anchor_id,
        caption.clone(),
        page_column,
        column_span,
        tag_role,
        outline.clone(),
    );
    let tail_block = make_table_block(
        x,
        column_widths,
        tail_rows,
        cell_padding,
        header_bg,
        border_color,
        border_thickness,
        border_style,
        edge,
        space_after,
        None,
        None,
        page_column,
        column_span,
        None,
        None,
    );
    SplitOutcome::Split(head_block, tail_block)
}

/// Split one row into a head + tail when even the row alone can't fit
/// in the remaining page space. Each cell is paginated independently
/// at the row's available inner height; cell-page-1 forms the head's
/// content for that cell, cell-page-2-onwards is concatenated as the
/// tail content.
///
/// Returns `None` when no split is possible (e.g. row content can't
/// be reduced — maybe a single image taller than `available`).
fn try_split_first_body_row(
    row: &TableRow,
    available: f32,
    cell_padding: f32,
) -> Option<(TableRow, TableRow)> {
    let inner = available - 2.0 * cell_padding;
    if inner <= 0.0 {
        return None;
    }

    let mut head_cells: Vec<TableCell> = Vec::with_capacity(row.cells.len());
    let mut tail_cells: Vec<TableCell> = Vec::with_capacity(row.cells.len());
    let mut any_split = false;
    let mut head_has_content = false;

    for cell in &row.cells {
        let (col, span, rowspan) = (cell.col, cell.colspan, cell.rowspan);
        if cell.blocks.is_empty() {
            head_cells.push(TableCell {
                blocks: Vec::new(),
                col,
                colspan: span,
                rowspan,
            });
            tail_cells.push(TableCell {
                blocks: Vec::new(),
                col,
                colspan: span,
                rowspan,
            });
            continue;
        }
        let pages = paginate_blocks(cell.blocks.clone(), inner);
        let mut iter = pages.into_iter();
        let head = iter.next().unwrap_or_default();
        let rest: Vec<Block> = iter.flatten().collect();
        if !head.is_empty() {
            head_has_content = true;
        }
        if !rest.is_empty() {
            any_split = true;
        }
        head_cells.push(TableCell {
            blocks: head,
            col,
            colspan: span,
            rowspan,
        });
        tail_cells.push(TableCell {
            blocks: rest,
            col,
            colspan: span,
            rowspan,
        });
    }

    if !any_split || !head_has_content {
        return None;
    }

    let head_height = cells_max_height(&head_cells) + 2.0 * cell_padding;
    let tail_height = cells_max_height(&tail_cells) + 2.0 * cell_padding;

    Some((
        TableRow {
            is_header: false,
            fill: row.fill,
            header_column: row.header_column,
            height: head_height,
            cells: head_cells,
        },
        TableRow {
            is_header: false,
            fill: row.fill,
            header_column: row.header_column,
            height: tail_height,
            cells: tail_cells,
        },
    ))
}

fn cells_max_height(cells: &[TableCell]) -> f32 {
    cells
        .iter()
        .map(|c| sum_block_height_slice(&c.blocks))
        .fold(0.0_f32, f32::max)
}

fn sum_block_height_slice(blocks: &[Block]) -> f32 {
    let total: f32 = blocks.iter().map(|b| b.height + b.space_after).sum();
    total - blocks.last().map(|b| b.space_after).unwrap_or(0.0)
}

#[allow(clippy::too_many_arguments)]
fn make_table_block(
    x: f32,
    column_widths: Vec<f32>,
    rows: Vec<TableRow>,
    cell_padding: f32,
    header_bg: rgb::Color,
    border_color: rgb::Color,
    border_thickness: f32,
    border_style: super::style::TableBorders,
    edge: Option<(rgb::Color, f32)>,
    space_after: f32,
    anchor_id: Option<String>,
    caption: Option<String>,
    page_column: u8,
    column_span: bool,
    tag_role: Option<TagRole>,
    outline: Option<OutlineEntry>,
) -> Block {
    let height: f32 =
        rows.iter().map(|r| r.height).sum::<f32>() + border_thickness * (rows.len() as f32 + 1.0);
    Block {
        height,
        space_after,
        draw: BlockDraw::Table {
            x,
            column_widths,
            rows,
            cell_padding,
            header_bg,
            border_color,
            border_thickness,
            border_style,
            edge,
            caption,
        },
        outline,
        anchor_id,

        tag_role,
        page_column,
        column_span,
    }
}

/// A decoded icon drawn at a [`BlockDraw::BoxedGroup`]'s top-left
/// content corner. Used by callouts; the box's children are laid out
/// indented past `size` so they clear it.
#[derive(Clone)]
pub struct BoxedGroupIcon {
    pub decoded: super::decoration::DecodedMedia,
    /// Absolute x of the icon's left edge (page-local).
    pub x: f32,
    /// Square draw size in points.
    pub size: f32,
}

/// Circle-badge geometry for an ordered-list marker, precomputed at
/// layout time so emit only has to stroke a circle and centre the
/// marker glyphs. Offsets are relative to the list item's
/// `(marker_x, block top)` origin.
#[derive(Clone, Copy)]
pub struct MarkerBadge {
    pub fill: rgb::Color,
    /// Circle diameter in points.
    pub diameter: f32,
    /// Circle-centre x, measured from `marker_x`.
    pub center_dx: f32,
    /// Circle-centre y, measured from the block's top y.
    pub center_dy: f32,
}

// `ListItem.marker` is a `Layout<rgb::Color>` (~333 bytes), much larger
// than other variants. Boxing it would force pagination/emit to chase a
// pointer for every list bullet — net loss for typical list-heavy docs.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum BlockDraw {
    /// A (possibly partial) parley text block. When `slice.line_range`
    /// covers all lines, this is the full paragraph; pagination may
    /// split into two `Text` blocks each pointing at the same
    /// underlying `Layout` via `Arc`, with disjoint line ranges.
    Text(TextSlice),
    /// A horizontal rule.
    Rule {
        x: f32,
        width: f32,
        thickness: f32,
        color: rgb::Color,
    },
    /// A box decoration: draws a background/border around the children,
    /// with optional left accent stripe, then renders the children inside
    /// (offset by `padding`). An optional `icon` is drawn at the box's
    /// top-left content corner (children are laid out indented past it).
    BoxedGroup {
        x: f32,
        width: f32,
        background: Option<rgb::Color>,
        border: Option<rgb::Color>,
        accent_left: Option<rgb::Color>,
        accent_width: f32,
        padding: f32,
        children: Vec<Block>,
        icon: Option<BoxedGroupIcon>,
        /// Optional horizontal rule `(colour, thickness)` drawn across
        /// the top / bottom edge of the box — the "bulletin" framing,
        /// used instead of a fill+border.
        top_rule: Option<(rgb::Color, f32)>,
        bottom_rule: Option<(rgb::Color, f32)>,
    },
    /// A list item: marker drawn at `marker_x`. Body blocks are
    /// pre-positioned at the indented content x (baked in at layout
    /// time), and share the marker's top y so the marker sits on the
    /// first body line's baseline. `ordered` controls the surrounding
    /// `L` tag's `/ListNumbering` attribute under PDF/UA: ordered
    /// items map to `Decimal`, unordered to `Disc`.
    ListItem {
        marker: Layout<rgb::Color>,
        marker_text: String,
        marker_x: f32,
        body: Vec<Block>,
        ordered: bool,
        /// When set, the marker is drawn centred inside a filled circle
        /// instead of as plain inline text.
        badge: Option<MarkerBadge>,
        /// When set, draw a vector checkmark in this colour instead of the
        /// text marker — for `{% list type="checkmark" %}`, whose `✓` glyph
        /// many fonts (incl. the default) lack.
        check: Option<rgb::Color>,
    },
    /// A pre-decoded raster image, drawn at `(x, y_top)` with the given
    /// display size. `caption` is used for the List of Figures.
    Image {
        image: KrillaImage,
        x: f32,
        width: f32,
        height: f32,
        caption: Option<String>,
    },
    /// A pre-parsed SVG tree. `caption` is used for the List of Figures.
    Svg {
        tree: Arc<SvgTree>,
        x: f32,
        width: f32,
        height: f32,
        caption: Option<String>,
    },
    /// Internal page-break marker. Carries no content; the paginator
    /// flushes the current page when it encounters one and drops the
    /// marker. Synthetic — sources can't produce one directly (the
    /// renderer materialises them from style-driven features like the
    /// cover page).
    PageBreak,
    /// A simple equal-width table with optional header row. `caption`
    /// is the caption text (used by the List of Tables when present).
    /// Visual rendering of the caption is a separate sibling block.
    Table {
        x: f32,
        column_widths: Vec<f32>,
        rows: Vec<TableRow>,
        cell_padding: f32,
        header_bg: rgb::Color,
        border_color: rgb::Color,
        border_thickness: f32,
        /// Which rules to draw (grid / horizontal-only / none).
        border_style: super::style::TableBorders,
        /// Optional outer-frame stroke `(color, thickness)` for the
        /// top/bottom (and grid left/right) rules; `None` draws edges in
        /// the internal border colour like the rest.
        edge: Option<(rgb::Color, f32)>,
        caption: Option<String>,
    },
    /// A floated image with content wrapping beside then below it — the
    /// `{% float %}` construct. `image` is a fully-positioned `Image`/`Svg`
    /// block (its `x` already placed on the chosen side); `wrap` is the
    /// following content as ordinary blocks, each laid out at the width
    /// available at its vertical position — narrowed and shifted while it
    /// overlaps the image, full-column once past it. Paragraphs that
    /// straddle the image's bottom edge reflow (narrow lines then full).
    /// The wrap keeps its real structure (lists, code, callouts, tables,
    /// nested figures) and its footnotes / anchors. The whole float is one
    /// indivisible pagination unit — it never straddles a page break (if it
    /// doesn't fit, it moves to the next page); `wrap` blocks stack from the
    /// float's top exactly as [`emit_blocks`](super::emit::emit_blocks) lays
    /// them, so the layout-time and emit-time y agree.
    Float { image: Box<Block>, wrap: Vec<Block> },
    /// A prose region flowing around several anchored floats — the
    /// multi-image `{% float %}` (a `float:left` up high, a `float:right`
    /// lower down, …). `text` is the single laid-out prose stream (its lines
    /// already narrowed / shifted per-line for whichever floats they overlap);
    /// `floats` are the images, each with its resolved position. Like
    /// [`Float`](Self::Float) it is one indivisible pagination unit.
    FloatRegion {
        text: TextSlice,
        floats: Vec<FloatItem>,
    },
    /// A print form field — the `{% input %}` tag. Pure graphics (a label
    /// above a ruled box), so it is PDF/A-safe: no interactive widget, no
    /// JavaScript. `label` sits at the block top; the field box is stroked at
    /// `(field_x, field_y)` (both relative to the block, `field_x` absolute
    /// x); an optional small grey `hint` (type / constraints) sits at
    /// `hint_y`. Field width follows `maxlength`; `type="checkbox"` is a small
    /// square.
    FormField {
        label: Option<TextSlice>,
        field_x: f32,
        field_y: f32,
        field_w: f32,
        field_h: f32,
        border: rgb::Color,
        thickness: f32,
        hint: Option<TextSlice>,
        hint_y: f32,
    },
}

/// One placed image inside a [`BlockDraw::FloatRegion`]: a fully-positioned
/// `Image`/`Svg` block (its `x` already absolute) drawn at `y_offset` below
/// the region's top.
#[derive(Clone)]
pub struct FloatItem {
    pub image: Box<Block>,
    pub y_offset: f32,
}

/// One laid-out table cell. Its content `blocks` already carry their `x`
/// (shifted into the cell's column at layout time). `col` is the 0-based
/// start column (cells aren't contiguous when a `rowspan` above leaves a
/// gap); `colspan`/`rowspan` are how many columns/rows it covers (≥ 1).
#[derive(Clone)]
pub struct TableCell {
    pub blocks: Vec<Block>,
    pub col: usize,
    pub colspan: usize,
    pub rowspan: usize,
}

#[derive(Clone)]
pub struct TableRow {
    pub is_header: bool,
    /// Optional background fill for zebra striping; `None` = no fill.
    /// Header rows leave this `None` (they paint the table header colour).
    pub fill: Option<rgb::Color>,
    /// Whether this table has a header column (column 0 = row headers).
    /// Constant across a table's rows; carried per-row so it survives
    /// pagination splits.
    pub header_column: bool,
    pub height: f32,
    /// The row's cells in order (a `colspan` cell covers several columns).
    pub cells: Vec<TableCell>,
}

/// A reference into a parley `Layout` that may cover all of it or a
/// contiguous sub-range of its lines. Splittable at line boundaries by
/// the paginator, with both halves sharing the same underlying data via
/// `Arc`.
#[derive(Clone)]
pub struct TextSlice {
    pub layout: Arc<Layout<rgb::Color>>,
    pub text: Arc<String>,
    pub links: Arc<Vec<LinkRange>>,
    /// Mid-paragraph anchor declarations — `{% tag id="X" %}` placed
    /// inside running text. The renderer maps each byte offset to a
    /// (line, y) when building the anchor map.
    pub mid_anchors: Arc<Vec<MidAnchor>>,
    /// Footnote call sites in this text — same byte offsets as the
    /// raw collected `text`, paired with the assigned 1-based number.
    /// Pagination uses these to determine which footnote bodies belong
    /// in each page's pool.
    pub footnote_calls: Arc<Vec<super::inline::FootnoteCall>>,
    /// Drawn height per line (ascent + descent + leading), in line order.
    pub line_heights: Arc<Vec<f32>>,
    pub x: f32,
    /// Half-open range of lines this slice draws.
    pub line_range: std::ops::Range<usize>,
    /// Y offset to subtract from baselines so the slice's first drawn
    /// line appears at the block's top y. Equals the cumulative
    /// `line_heights[..line_range.start]`.
    pub skip_y: f32,
}

impl TextSlice {
    /// Build the canonical "whole paragraph" slice — covers every line.
    pub fn whole(layout: Layout<rgb::Color>, text: String, links: Vec<LinkRange>, x: f32) -> Self {
        Self::whole_with_anchors(layout, text, links, Vec::new(), x)
    }

    /// Same as `whole` but with mid-paragraph anchor declarations.
    pub fn whole_with_anchors(
        layout: Layout<rgb::Color>,
        text: String,
        links: Vec<LinkRange>,
        mid_anchors: Vec<MidAnchor>,
        x: f32,
    ) -> Self {
        Self::whole_with_extras(layout, text, links, mid_anchors, Vec::new(), x)
    }

    /// Full constructor — includes footnote call sites collected by
    /// `Inlines`.
    pub fn whole_with_extras(
        layout: Layout<rgb::Color>,
        text: String,
        links: Vec<LinkRange>,
        mid_anchors: Vec<MidAnchor>,
        footnote_calls: Vec<super::inline::FootnoteCall>,
        x: f32,
    ) -> Self {
        let line_heights: Vec<f32> = layout
            .lines()
            .map(|l| {
                let m = l.metrics();
                m.ascent + m.descent + m.leading
            })
            .collect();
        let n = line_heights.len();
        Self {
            layout: Arc::new(layout),
            text: Arc::new(text),
            links: Arc::new(links),
            mid_anchors: Arc::new(mid_anchors),
            footnote_calls: Arc::new(footnote_calls),
            line_heights: Arc::new(line_heights),
            x,
            line_range: 0..n,
            skip_y: 0.0,
        }
    }

    /// Drawn height of just this slice.
    pub fn height(&self) -> f32 {
        self.line_heights[self.line_range.clone()]
            .iter()
            .copied()
            .sum()
    }

    /// Footnote numbers whose call mark falls within this slice's
    /// drawn line range. A split paragraph thereby attributes each
    /// footnote to the slice that actually shows the call mark.
    pub fn footnote_numbers(&self) -> Vec<u32> {
        if self.footnote_calls.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for call in self.footnote_calls.iter() {
            if let Some(line) = find_line_for_byte(&self.layout, call.byte_offset)
                && line >= self.line_range.start
                && line < self.line_range.end
            {
                out.push(call.number);
            }
        }
        out
    }
}

fn find_line_for_byte(layout: &Layout<rgb::Color>, byte: usize) -> Option<usize> {
    let mut last = None;
    for (i, line) in layout.lines().enumerate() {
        last = Some(i);
        for run in line.runs() {
            for cluster in run.visual_clusters() {
                let r = cluster.text_range();
                if byte >= r.start && byte < r.end {
                    return Some(i);
                }
                if byte == r.end {
                    return Some(i);
                }
            }
        }
    }
    last
}

/// Build the block list for one page's footnote pool: a separator
/// rule followed by one Text block per `(number, body)` entry, in
/// number order. `inner_x` and `inner_w` are the body content's
/// horizontal extents; the rule is anchored at `inner_x` with the
/// width given by `style.footnote.rule_width_frac`.
///
/// Returns an empty `Vec` when `entries` is empty so the caller can
/// add zero pool height when no footnotes hit this page.
pub fn build_footnote_pool_blocks(
    entries: &[(u32, String)],
    style: &Style,
    body_families: &'static [&'static str],
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<rgb::Color>,
    inner_x: f32,
    inner_w: f32,
) -> Vec<Block> {
    if entries.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();

    // 1. Separator rule. Implemented as a thin Rule block with leading
    //    `gap_above` baked into the previous block's `space_after` —
    //    here we render only what sits below the body content.
    out.push(Block {
        height: style.footnote.gap_above,
        space_after: 0.0,
        draw: BlockDraw::Rule {
            x: inner_x,
            width: 0.0,
            thickness: 0.0,
            color: style.footnote.rule_color.into(),
        },
        outline: None,
        anchor_id: None,

        tag_role: None,
        page_column: 0,
        column_span: false,
    });
    let rule_w = (inner_w * style.footnote.rule_width_frac).max(0.0);
    out.push(Block {
        height: style.footnote.rule_thickness.max(0.5),
        space_after: style.footnote.gap_below_rule,
        draw: BlockDraw::Rule {
            x: inner_x,
            width: rule_w,
            thickness: style.footnote.rule_thickness,
            color: style.footnote.rule_color.into(),
        },
        outline: None,
        anchor_id: None,

        tag_role: None,
        page_column: 0,
        column_span: false,
    });

    // 2. One paragraph per footnote: "ⁿ body text".
    let entry_style = TextStyle {
        font_size: style.footnote.font_size,
        font_weight: 400.0,
        line_height: style.footnote.line_height,
        color: style.footnote.text_color.into(),
        font_families: body_families,
        italic: false,
    };
    let last = entries.len().saturating_sub(1);
    for (i, (number, body)) in entries.iter().enumerate() {
        let mark = super::inline::superscript_number(*number);
        let text = format!("{mark} {}", body);
        let layout = build_layout(&text, &[], &entry_style, inner_w, font_cx, layout_cx);
        let slice = TextSlice::whole(layout, text, Vec::new(), inner_x);
        let h = slice.height();
        out.push(Block {
            height: h,
            space_after: if i == last {
                0.0
            } else {
                style.footnote.entry_space_after
            },
            draw: BlockDraw::Text(slice),
            outline: None,
            anchor_id: None,
            tag_role: Some(TagRole::Note),
            page_column: 0,
            column_span: false,
        });
    }
    out
}

/// Total drawn height of a slice of pool blocks (sum of height +
/// space_after, minus the trailing block's space_after).
pub fn pool_height(pool: &[Block]) -> f32 {
    if pool.is_empty() {
        return 0.0;
    }
    let mut h = 0.0;
    for (i, b) in pool.iter().enumerate() {
        h += b.height;
        if i + 1 < pool.len() {
            h += b.space_after;
        }
    }
    h
}

/// Layout context passed through the recursion. Keeps it tidy.
pub struct LayoutCtx<'a> {
    pub style: &'a Style,
    pub font_cx: &'a mut FontContext,
    pub layout_cx: &'a mut LayoutContext<rgb::Color>,
    pub assets: &'a dyn AssetResolver,
    /// Monotonically incrementing counter used to assign synthetic
    /// anchor ids to headings that don't have an explicit `{% tag %}`.
    pub next_heading: usize,
    /// Counter for synthetic figure anchors (`__figure_<n>`).
    pub next_figure: usize,
    /// Counter for synthetic table anchors (`__table_<n>`).
    pub next_table: usize,
    /// When `layout_children` encounters `{% caption %}` immediately
    /// followed by a `<table>`, it stashes the caption text here so
    /// `layout_table` can pick it up and store it on the table block
    /// for the List of Tables.
    pub pending_table_caption: Option<String>,
    /// Same idea for figures — `{% caption %}...{% /caption %}` placed
    /// just before an image / media tag attaches as that figure's
    /// caption, overriding any alt-derived default.
    pub pending_figure_caption: Option<String>,
    /// Footnote registry — a counter for the next footnote number plus
    /// the body text for each registered footnote, indexed by number-1.
    /// Inline collection registers each `{% footnote %}` body and gets
    /// back a sequential number; pagination later pulls bodies by
    /// number to compose the per-page footnote pool.
    pub footnotes: Vec<String>,
    /// Effective body font family list, resolved from
    /// `Style::body_font_families` (or the bundled Noto defaults when
    /// empty). Layout sites use this in place of `default_families()`
    /// so caller-specified custom fonts apply to all body text.
    pub body_families: &'a [&'static str],
    /// Optional word hyphenator. When present, plain-text body
    /// paragraphs (no bold/italic/links/anchors/footnotes) get soft
    /// hyphens inserted before parley layout so wrapped lines can
    /// break inside long words. Paragraphs with inline markup are
    /// skipped to avoid drifting byte offsets in their ranges.
    pub hyphenator: Option<&'a super::hyphen::WordHyphenator>,
    /// Running per-level counters for automatic heading numbering,
    /// indexed by `level - 1` (so `[0]` is h1). Only meaningful when
    /// `style.heading_numbering.enabled`. A heading at level L bumps
    /// `counters[L-1]` and zeroes every deeper level.
    pub heading_counters: [u32; 6],
    /// Current ordered/unordered list nesting depth (0 at the outermost
    /// list). Drives depth-cycled ordered numbering (`1.` → `a.` → `i.`).
    pub list_depth: usize,
    /// Transient horizontal alignment applied to cell content — set by
    /// `{% columns align=… %}` / `{% grid align=… %}` around their table
    /// layout so paragraphs and images inside each cell centre / right-align
    /// (the normal `align` column mechanism only reaches single-paragraph
    /// cells; grid cells hold an image plus a headline plus body). `None`
    /// everywhere else, so ordinary paragraphs and figures are unaffected.
    pub cell_content_align: Option<parley::layout::Alignment>,
    /// Precomputed `{% tag %}` id → display label for `{% tagref %}`
    /// (e.g. `"6.5 Manual brake release"`). Built once before layout so
    /// forward references resolve correctly.
    pub crossref_labels: &'a HashMap<String, String>,
}

impl<'a> LayoutCtx<'a> {
    pub fn next_heading_id(&mut self) -> usize {
        let n = self.next_heading;
        self.next_heading += 1;
        n
    }
    pub fn next_figure_id(&mut self) -> usize {
        let n = self.next_figure;
        self.next_figure += 1;
        n
    }
    pub fn next_table_id(&mut self) -> usize {
        let n = self.next_table;
        self.next_table += 1;
        n
    }

    /// Advance the heading counters for a numbered heading at `level`
    /// and return the formatted prefix (e.g. `"1.2.1"`), without the
    /// trailing separator. Returns `None` when numbering is disabled or
    /// `level` is deeper than the configured `max_depth`.
    fn bump_heading_number(&mut self, level: u8) -> Option<String> {
        let cfg = &self.style.heading_numbering;
        if !cfg.enabled {
            return None;
        }
        bump_heading_counters(&mut self.heading_counters, level, cfg.max_depth)
    }

    pub fn page_columns(&self) -> u8 {
        self.style.page_layout.columns.max(1)
    }

    pub fn body_full_width(&self) -> f32 {
        self.style.page_width - 2.0 * self.style.margin_x
    }

    pub fn body_column_width(&self) -> f32 {
        let cols = self.page_columns();
        let full = self.body_full_width();
        if cols <= 1 {
            return full;
        }
        let gap = self.style.page_layout.gap;
        (full - gap * (cols as f32 - 1.0)) / cols as f32
    }

    pub fn heading_spans_columns(&self, level: u8) -> bool {
        self.page_columns() > 1 && level <= self.style.page_layout.span_heading_through
    }
}

/// Advance `counters` for a heading at `level` (1-based) and format the
/// dotted prefix. Returns `None` when `level` is 0 or deeper than
/// `max_depth` (clamped to `1..=6`).
///
/// Bumping a level resets all deeper levels, so a fresh `h2` after
/// `1.3.4` yields `1.4` rather than `1.4.4`. Pulled out as a free
/// function so the counter arithmetic is unit-testable without building
/// a whole `LayoutCtx`, and reusable by the pre-layout crossref label
/// pass.
pub(crate) fn bump_heading_counters(
    counters: &mut [u32; 6],
    level: u8,
    max_depth: u8,
) -> Option<String> {
    if level == 0 || level > max_depth.clamp(1, 6) {
        return None;
    }
    let idx = (level - 1) as usize;
    counters[idx] += 1;
    for deeper in &mut counters[idx + 1..] {
        *deeper = 0;
    }
    let prefix = counters[..=idx]
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".");
    Some(prefix)
}

/// Used as a stand-in resolver when the caller hasn't supplied one.
/// Documents that reference media will render placeholders.
pub fn null_resolver() -> &'static dyn AssetResolver {
    static NULL: NullAssetResolver = NullAssetResolver;
    &NULL
}

/// Top-level entry: lay out a transformed Markdoc document into blocks.
pub fn layout_document(root: &RenderableTreeNode, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let column_x = ctx.style.margin_x;
    let column_w = ctx.body_column_width();
    layout_node(root, column_x, column_w, ctx)
}

/// Lay out a single node into 0+ blocks.
///
/// `x` is the absolute left edge for any text in this subtree.
/// `width` is the available text-column width.
fn layout_node(
    node: &RenderableTreeNode,
    x: f32,
    width: f32,
    ctx: &mut LayoutCtx<'_>,
) -> Vec<Block> {
    let RenderableTreeNode::Tag(tag) = node else {
        return Vec::new();
    };

    match tag.name.as_str() {
        // Document / generic structural wrappers: recurse children.
        "div" => layout_children(&tag.children, x, width, ctx),

        "p" => layout_paragraph(tag, x, width, ctx),
        h @ ("h1" | "h2" | "h3" | "h4" | "h5" | "h6") => {
            let level = h.as_bytes()[1] - b'0';
            layout_heading(tag, level, x, width, ctx)
        }

        "ul" => layout_list(tag, ListKind::Unordered, None, x, width, ctx),
        "ol" => layout_list(tag, ListKind::Ordered, None, x, width, ctx),

        // `{% list type="checkmark" %}` — render the wrapped list with a
        // custom marker (checkmark / dash / none).
        "list" => layout_list_tag(tag, x, width, ctx),

        "blockquote" => layout_blockquote(tag, x, width, ctx),

        "pre" => layout_code_block(tag, x, width, ctx),

        "callout" => layout_callout(tag, x, width, ctx),

        "table" => layout_table(tag, x, width, ctx),

        // `{% columns %}` — place children side by side in equal columns.
        "columns" => layout_columns(tag, x, width, ctx),

        // `{% grid %}` — cells reflow into as many equal columns as fit.
        "grid" => layout_grid(tag, x, width, ctx),

        // `{% swatch %}` — a block colour bar / chip (solid or gradient).
        "swatch" => layout_swatch(tag, x, width, ctx),

        // `{% lightindicators /%}` — status-light legend (swatch grid).
        "lightindicators" => layout_lightindicators(x, width, ctx),

        // `{% noParaSpaceBox %}` — collapse inter-paragraph gaps (address /
        // contact stacks in copyright).
        "noParaSpaceBox" => layout_no_para_space_box(tag, x, width, ctx),

        // `{% imagegrid %}` / `{% gridimage %}` — illustrated side-by-side
        // cells on a light grey panel (Locomotion, etc.).
        "imagegrid" => layout_imagegrid(tag, x, width, ctx),
        "gridimage" => layout_gridimage(tag, x, width, ctx),

        // `{% qr %}` — a QR code from `value`.
        "qr" => layout_qr(tag, x, width, ctx),

        // `{% float %}` — an image on one side with prose wrapping around it.
        "float" => layout_float(tag, x, width, ctx),

        // `{% input %}` — a print form field (label + ruled box).
        "input" => layout_input(tag, x, width, ctx),

        "img" | "media" => layout_media(tag, x, width, ctx),

        // `{% toc /%}` marks where a start-positioned table of contents
        // should be inserted (front matter before it, body after). The
        // marker block renders nothing; the renderer locates its page to
        // pick the split point. A trailing page break starts the body on
        // a fresh page after the ToC.
        "toc" => vec![toc_marker_block(x), page_break_block()],

        // `{% pagebreak /%}` forces the following content onto a new page.
        "pagebreak" => vec![page_break_block()],

        "hr" => vec![layout_rule(x, width, ctx.style)],

        // Inline-only tags should never appear at block level (`a`, `strong`
        // etc.); if they do, just recurse children defensively so content
        // isn't lost.
        _ => layout_children(&tag.children, x, width, ctx),
    }
}

fn layout_children(
    children: &[RenderableTreeNode],
    x: f32,
    width: f32,
    ctx: &mut LayoutCtx<'_>,
) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < children.len() {
        // Detect `{% caption %}...{% /caption %}` directly followed by
        // either a `<table>` or an `<img>` / `<media>` (possibly
        // wrapped in a `<p>`). Render the caption as a small text
        // block above the figure/table, AND stash the caption text so
        // the corresponding layout (table / media) attaches it for
        // LoT / LoF entries.
        if let Some((cap_text, target, override_pos, override_color)) =
            caption_before_target(children, i)
        {
            let caption_block = build_caption_block(
                &caption_inlines_for_node(&children[i]),
                x,
                width,
                override_color,
                ctx,
            );
            match target {
                CaptionTarget::Table => ctx.pending_table_caption = Some(cap_text.clone()),
                CaptionTarget::Figure => ctx.pending_figure_caption = Some(cap_text.clone()),
            }
            let target_blocks = layout_node(&children[i + 1], x, width, ctx);
            // Per-caption attribute wins over the global style setting.
            let pos = override_pos.unwrap_or(ctx.style.caption_position);
            match pos {
                super::style::CaptionPosition::Above => {
                    out.push(caption_block);
                    out.extend(target_blocks);
                }
                super::style::CaptionPosition::Below => {
                    out.extend(target_blocks);
                    out.push(caption_block);
                }
            }
            i += 2;
            continue;
        }
        out.extend(layout_node(&children[i], x, width, ctx));
        i += 1;
    }
    out
}

/// What follows a caption: either a table or a figure (image / media).
#[derive(Copy, Clone)]
enum CaptionTarget {
    Table,
    Figure,
}

/// Lay out the visible caption text as a small italic block at column x.
fn build_caption_block(
    inlines: &Inlines,
    x: f32,
    width: f32,
    color_override: Option<krilla::color::rgb::Color>,
    ctx: &mut LayoutCtx<'_>,
) -> Block {
    // Per-caption `color` wins, then the document `caption_color`, then the
    // historical fallback of the block-quote text colour.
    let color = color_override
        .or_else(|| ctx.style.caption_color.map(|c| c.into()))
        .unwrap_or_else(|| ctx.style.blockquote_text_color.into());
    let style = TextStyle {
        font_size: ctx.style.body_font_size * 0.92,
        font_weight: 400.0,
        line_height: ctx.style.body_line_height,
        color,
        font_families: ctx.body_families,
        italic: true,
    };
    let layout = build_layout(
        &inlines.text,
        &inlines.style_ranges,
        &style,
        width,
        ctx.font_cx,
        ctx.layout_cx,
    );
    let slice = TextSlice::whole_with_anchors(
        layout,
        inlines.text.clone(),
        inlines.links.clone(),
        inlines.mid_anchors.clone(),
        x,
    );
    let height = slice.height();
    Block {
        height,
        space_after: ctx.style.paragraph_space_after * 0.5,
        draw: BlockDraw::Text(slice),
        outline: None,
        anchor_id: None,

        tag_role: None,
        page_column: 0,
        column_span: false,
    }
}

/// A visible caption block from a plain string — the value of a
/// `{% media caption="…" /%}` attribute (no inline markup). Mirrors
/// [`build_caption_block`]'s small-italic look and `caption_color`.
fn caption_block_from_str(text: &str, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Block {
    let color = ctx
        .style
        .caption_color
        .map(|c| c.into())
        .unwrap_or_else(|| ctx.style.blockquote_text_color.into());
    let style = TextStyle {
        font_size: ctx.style.body_font_size * 0.92,
        font_weight: 400.0,
        line_height: ctx.style.body_line_height,
        color,
        font_families: ctx.body_families,
        italic: true,
    };
    let layout = build_layout(text, &[], &style, width, ctx.font_cx, ctx.layout_cx);
    let slice = TextSlice::whole(layout, text.to_string(), Vec::new(), x);
    let height = slice.height();
    Block {
        height,
        space_after: ctx.style.paragraph_space_after * 0.5,
        draw: BlockDraw::Text(slice),
        outline: None,
        anchor_id: None,
        tag_role: None,
        page_column: 0,
        column_span: false,
    }
}

/// Attach a `visible_caption` (from a `{% media caption="…" /%}` attribute)
/// beneath or above a laid-out media `image` block, per the style's
/// `caption_position`. `None` returns the image alone.
fn finish_media(
    image: Block,
    visible_caption: Option<String>,
    x: f32,
    width: f32,
    ctx: &mut LayoutCtx<'_>,
) -> Vec<Block> {
    let Some(text) = visible_caption else {
        return vec![image];
    };
    let cap = caption_block_from_str(&text, x, width, ctx);
    match ctx.style.caption_position {
        super::style::CaptionPosition::Above => vec![cap, image],
        super::style::CaptionPosition::Below => vec![image, cap],
    }
}

/// If `children[i]` is a caption tag (or a paragraph wrapping one) and
/// `children[i+1]` is a captionable target (table or figure, possibly
/// wrapped in a `<p>`), return the caption's collected inline text,
/// the target kind, and optional per-caption overrides for position
/// (`{% caption position="above"|"below" %}`) and colour
/// (`{% caption color="#…" %}`).
fn caption_before_target(
    children: &[RenderableTreeNode],
    i: usize,
) -> Option<(
    String,
    CaptionTarget,
    Option<super::style::CaptionPosition>,
    Option<krilla::color::rgb::Color>,
)> {
    let cap = find_caption_tag(children.get(i)?)?;
    let next = children.get(i + 1)?;
    let target = classify_caption_target(next)?;
    let mut text = String::new();
    let mut ranges = Vec::new();
    // Captions don't carry footnotes — pass a throwaway registry so
    // any stray `{% footnote %}` inside is silently dropped without
    // affecting the document's footnote numbering.
    collect_inlines(&mut text, &mut ranges, &cap.children, &mut Vec::new());
    if text.trim().is_empty() {
        return None;
    }
    let override_pos = caption_position_attr(cap);
    let override_color = caption_color_attr(cap);
    Some((text, target, override_pos, override_color))
}

/// Read an optional `position="above"|"below"` attribute from a
/// `{% caption %}` tag; unknown values are ignored.
fn caption_position_attr(cap: &Tag) -> Option<super::style::CaptionPosition> {
    match cap.attributes.get("position") {
        Some(Scalar::String(s)) => match s.as_str() {
            "above" => Some(super::style::CaptionPosition::Above),
            "below" => Some(super::style::CaptionPosition::Below),
            _ => None,
        },
        _ => None,
    }
}

/// Read an optional `color="#rrggbb"` (or CSS named colour) attribute from a
/// `{% caption %}` tag; unparsable values are ignored.
fn caption_color_attr(cap: &Tag) -> Option<krilla::color::rgb::Color> {
    match cap.attributes.get("color") {
        Some(Scalar::String(s)) => super::inline::parse_css_color(s),
        _ => None,
    }
}

fn classify_caption_target(node: &RenderableTreeNode) -> Option<CaptionTarget> {
    if contains_top_table(node) {
        return Some(CaptionTarget::Table);
    }
    if contains_top_figure(node) {
        return Some(CaptionTarget::Figure);
    }
    None
}

fn contains_top_figure(node: &RenderableTreeNode) -> bool {
    if let RenderableTreeNode::Tag(t) = node {
        if t.name == "img" || t.name == "media" {
            return true;
        }
        if t.name == "p" || t.name == "div" {
            return t.children.iter().any(contains_top_figure);
        }
    }
    false
}

/// Return the inner `<caption>` tag whether the node is the tag itself
/// or a `<p>` wrapping only that tag.
fn find_caption_tag(node: &RenderableTreeNode) -> Option<&Tag> {
    if let RenderableTreeNode::Tag(t) = node {
        if t.name == "caption" {
            return Some(t.as_ref());
        }
        if t.name == "p" {
            // Only accept paragraphs that contain a single caption tag
            // (plus possible softbreaks) — anything else is a real
            // paragraph that should not be repurposed.
            let mut found: Option<&Tag> = None;
            for c in &t.children {
                match c {
                    RenderableTreeNode::Tag(inner) => {
                        if inner.name == "caption" && found.is_none() {
                            found = Some(inner.as_ref());
                        } else if matches!(inner.name.as_str(), "softbreak" | "hardbreak" | "br") {
                            continue;
                        } else {
                            return None;
                        }
                    }
                    RenderableTreeNode::Scalar(_) => return None,
                }
            }
            return found;
        }
    }
    None
}

/// Check whether `node` is a `<table>` or a wrapper that contains one
/// directly.
fn contains_top_table(node: &RenderableTreeNode) -> bool {
    if let RenderableTreeNode::Tag(t) = node {
        if t.name == "table" {
            return true;
        }
        if t.name == "p" || t.name == "div" {
            return t.children.iter().any(contains_top_table);
        }
    }
    false
}

fn caption_inlines_for_node(node: &RenderableTreeNode) -> Inlines {
    if let Some(cap) = find_caption_tag(node) {
        // Captions don't carry document footnotes; throwaway registry.
        Inlines::from(&cap.children, &mut Vec::new())
    } else {
        Inlines::new()
    }
}

// ── Paragraph ───────────────────────────────────────────────────────────

fn layout_paragraph(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    // Markdown / Markdoc both wrap some block-level constructs inside
    // a `<p>` produced by pulldown-cmark:
    //   - `![alt](url)` becomes `<p><img></p>` / `<p><media></p>`.
    //   - `{% callout %}…{% /callout %}` written on its own line ends up
    //     inside a synthetic paragraph because the tokeniser sees it as
    //     a regular inline run.
    // Promote any such direct child to its block-level layout so it
    // actually renders as a box/image instead of being flattened into
    // the inline text by the `Inlines` collector. Remaining children
    // flow as a normal paragraph.
    let mut promoted: Vec<Block> = Vec::new();
    let mut text_children: Vec<RenderableTreeNode> = Vec::new();
    for child in &tag.children {
        if let RenderableTreeNode::Tag(t) = child {
            match t.name.as_str() {
                "img" | "media" => {
                    promoted.extend(layout_media(t, x, width, ctx));
                    continue;
                }
                "callout" => {
                    promoted.extend(layout_callout(t, x, width, ctx));
                    continue;
                }
                "input" => {
                    promoted.extend(layout_input(t, x, width, ctx));
                    continue;
                }
                _ => {}
            }
        }
        text_children.push(child.clone());
    }

    let mut out = promoted;

    let mut inlines =
        Inlines::from_with_labels(&text_children, &mut ctx.footnotes, Some(ctx.crossref_labels));
    if inlines.text.trim().is_empty() {
        return out;
    }
    // Style link text so readers spot it before they hover. The PDF
    // annotation is already created from `inlines.links`; this just
    // restyles the underlying glyphs. Done before hyphenation: links
    // pin the byte ranges anyway, so the soft-hyphen guard below
    // already skips paragraphs that contain any link.
    let link_style = &ctx.style.link;
    let link_color: krilla::color::rgb::Color = link_style.color.into();
    for link in &inlines.links {
        inlines.style_ranges.push(InlineRange {
            start: link.start,
            end: link.end,
            prop: InlineProp::Color(link_color),
        });
        if link_style.italic {
            inlines.style_ranges.push(InlineRange {
                start: link.start,
                end: link.end,
                prop: InlineProp::Italic,
            });
        }
        if link_style.bold {
            inlines.style_ranges.push(InlineRange {
                start: link.start,
                end: link.end,
                prop: InlineProp::Bold,
            });
        }
    }
    // Express the link underline as a parley decoration over the link's
    // byte range, so the unified decoration pass draws it. Skipped when the
    // style disables underlining; the colour follows the link text tint
    // pushed above.
    if link_style.underline {
        for link in &inlines.links {
            inlines.style_ranges.push(InlineRange {
                start: link.start,
                end: link.end,
                prop: InlineProp::Underline {
                    thickness: link_style.underline_thickness,
                },
            });
        }
    }
    // Hyphenate plain paragraphs only — inline ranges, links, anchors
    // and footnote calls all key on byte offsets, and inserting soft
    // hyphens shifts those, so we skip the pass when any are present.
    if let Some(h) = ctx.hyphenator
        && inlines.style_ranges.is_empty()
        && inlines.links.is_empty()
        && inlines.mid_anchors.is_empty()
        && inlines.footnote_calls.is_empty()
    {
        inlines.text = h.hyphenate(&inlines.text);
    }
    let style = TextStyle {
        font_size: ctx.style.body_font_size,
        font_weight: 400.0,
        line_height: ctx.style.body_line_height,
        color: ctx.style.text_color.into(),
        font_families: ctx.body_families,
        italic: false,
    };
    let layout = build_layout_aligned(
        &inlines.text,
        &inlines.style_ranges,
        &style,
        width,
        ctx.cell_content_align
            .unwrap_or_else(|| ctx.style.text_align.to_parley()),
        ctx.font_cx,
        ctx.layout_cx,
    );
    let slice = TextSlice::whole_with_extras(
        layout,
        inlines.text,
        inlines.links,
        inlines.mid_anchors,
        inlines.footnote_calls,
        x,
    );
    let height = slice.height();
    out.push(Block {
        height,
        space_after: ctx.style.paragraph_space_after,
        draw: BlockDraw::Text(slice),
        outline: None,
        anchor_id: None,

        tag_role: None,
        page_column: 0,
        column_span: false,
    });
    out
}

// ── Table of contents ───────────────────────────────────────────────────

/// One entry to render in the generated TOC. `text` is the heading
/// text; `level` is its heading level; `target_anchor_id` is the
/// auto-assigned anchor id of the heading (so the entry can hyperlink);
/// `page_number` is the 1-indexed PDF page number to display.
#[derive(Debug, Clone)]
pub struct TocEntry {
    pub level: u8,
    pub text: String,
    pub target_anchor_id: String,
    pub page_number: usize,
}

/// Format a single TOC entry with leader dots filling the gap between
/// the title and the page number:
///
///   `Title ......................... 12`
///
/// Falls back to a plain "Title  N" form when the title alone overflows
/// the available width (in which case the entry will wrap to multiple
/// lines and leader dots wouldn't make sense).
fn format_toc_entry_with_leaders(
    title: &str,
    page_number: usize,
    text_style: &TextStyle<'_>,
    available_width: f32,
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<rgb::Color>,
) -> String {
    let page_str = page_number.to_string();
    let title_w = measure_first_line_width(title, text_style, font_cx, layout_cx);
    let page_w = measure_first_line_width(&page_str, text_style, font_cx, layout_cx);
    // Reserve a small visual gap on each side of the dot run.
    let gap = measure_first_line_width(" ", text_style, font_cx, layout_cx) * 2.0;
    let dot_w = measure_first_line_width(".", text_style, font_cx, layout_cx);

    if dot_w <= 0.0 || title_w + page_w + gap >= available_width {
        // Title doesn't leave room for any dots — fall back to a plain
        // two-piece line; parley will wrap if it has to.
        return format!("{title}  {page_str}");
    }
    let dot_room = available_width - title_w - page_w - gap;
    let n_dots = (dot_room / dot_w).floor() as usize;
    if n_dots == 0 {
        return format!("{title}  {page_str}");
    }
    let dots: String = std::iter::repeat_n('.', n_dots).collect();
    format!("{title} {dots} {page_str}")
}

/// Build the TOC blocks: a title heading (h1-style), then one Block
/// per entry. Each entry is a single Text block with a clickable
/// internal link covering the entry text.
pub fn build_toc_blocks(
    entries: &[TocEntry],
    style: &Style,
    body_families: &'static [&'static str],
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<rgb::Color>,
) -> Vec<Block> {
    let mut blocks = Vec::new();
    let column_x = style.margin_x;
    let column_w = style.page_width - 2.0 * style.margin_x;

    // Title block (skipped when the title is empty).
    if !style.toc.title.is_empty() {
        let title_style = TextStyle {
            font_size: style.toc.title_font_size,
            font_weight: 700.0,
            line_height: style.body_line_height,
            color: style.text_color.into(),
            font_families: body_families,
            italic: false,
        };
        let title_layout = build_layout(
            &style.toc.title,
            &[],
            &title_style,
            column_w,
            font_cx,
            layout_cx,
        );
        let title_slice =
            TextSlice::whole(title_layout, style.toc.title.clone(), Vec::new(), column_x);
        blocks.push(Block {
            height: title_slice.height(),
            space_after: 18.0,
            draw: BlockDraw::Text(title_slice),
            // A top-level outline entry so the section's own title drives
            // the running header (and bookmark) on its page rather than
            // the previous chapter bleeding through.
            outline: Some(OutlineEntry {
                level: 1,
                text: style.toc.title.clone(),
            }),
            anchor_id: None,

            tag_role: None,
            page_column: 0,
            column_span: style.page_layout.columns > 1,
        });
    }

    // Per-entry blocks.
    let entry_text_style = TextStyle {
        font_size: style.toc.entry_font_size,
        font_weight: 400.0,
        line_height: style.body_line_height,
        color: style.text_color.into(),
        font_families: body_families,
        italic: false,
    };
    for entry in entries {
        if entry.level > style.toc.max_depth {
            continue;
        }
        let indent = (entry.level.saturating_sub(1) as f32) * style.toc.entry_indent_per_level;
        let entry_x = column_x + indent;
        let entry_w = column_w - indent;

        let body = format_toc_entry_with_leaders(
            &entry.text,
            entry.page_number,
            &entry_text_style,
            entry_w,
            font_cx,
            layout_cx,
        );

        let layout = build_layout(&body, &[], &entry_text_style, entry_w, font_cx, layout_cx);
        // Whole entry is clickable.
        let links = vec![LinkRange {
            start: 0,
            end: body.len(),
            href: format!("#{}", entry.target_anchor_id),
            title: None,
        }];
        let slice = TextSlice::whole(layout, body, links, entry_x);
        blocks.push(Block {
            height: slice.height(),
            space_after: style.toc.entry_space_after,
            draw: BlockDraw::Text(slice),
            outline: None,
            anchor_id: None,

            tag_role: None,
            page_column: 0,
            column_span: style.page_layout.columns > 1,
        });
    }

    blocks
}

/// Like `build_toc_blocks` but for flat list sections (List of Figures,
/// List of Tables). Title and font sizes come from a `ListSectionStyle`
/// instead of `TocStyle`. Entries don't indent by level.
#[allow(clippy::too_many_arguments)]
pub fn build_list_section_blocks(
    title: &str,
    title_font_size: f32,
    entry_font_size: f32,
    entry_space_after: f32,
    entries: &[TocEntry],
    style: &Style,
    body_families: &'static [&'static str],
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<rgb::Color>,
) -> Vec<Block> {
    let mut blocks = Vec::new();
    let column_x = style.margin_x;
    let column_w = style.page_width - 2.0 * style.margin_x;

    if !title.is_empty() {
        let title_style = TextStyle {
            font_size: title_font_size,
            font_weight: 700.0,
            line_height: style.body_line_height,
            color: style.text_color.into(),
            font_families: body_families,
            italic: false,
        };
        let title_layout = build_layout(title, &[], &title_style, column_w, font_cx, layout_cx);
        let title_slice = TextSlice::whole(title_layout, title.to_string(), Vec::new(), column_x);
        blocks.push(Block {
            height: title_slice.height(),
            space_after: 18.0,
            draw: BlockDraw::Text(title_slice),
            // Top-level outline entry so this section's title drives its
            // own running header rather than inheriting the prior chapter.
            outline: Some(OutlineEntry {
                level: 1,
                text: title.to_string(),
            }),
            anchor_id: None,

            tag_role: None,
            page_column: 0,
            column_span: style.page_layout.columns > 1,
        });
    }

    let entry_text_style = TextStyle {
        font_size: entry_font_size,
        font_weight: 400.0,
        line_height: style.body_line_height,
        color: style.text_color.into(),
        font_families: body_families,
        italic: false,
    };
    for entry in entries {
        let body = format_toc_entry_with_leaders(
            &entry.text,
            entry.page_number,
            &entry_text_style,
            column_w,
            font_cx,
            layout_cx,
        );
        let layout = build_layout(&body, &[], &entry_text_style, column_w, font_cx, layout_cx);
        let links = vec![LinkRange {
            start: 0,
            end: body.len(),
            href: format!("#{}", entry.target_anchor_id),
            title: None,
        }];
        let slice = TextSlice::whole(layout, body, links, column_x);
        blocks.push(Block {
            height: slice.height(),
            space_after: entry_space_after,
            draw: BlockDraw::Text(slice),
            outline: None,
            anchor_id: None,

            tag_role: None,
            page_column: 0,
            column_span: style.page_layout.columns > 1,
        });
    }
    blocks
}

// ── Heading ─────────────────────────────────────────────────────────────

/// True when tag `t` carries `key` set to a falsey scalar — `false`
/// (boolean), `"false"`, or `"0"`. Used to read `numbered="false"` off
/// a heading's anchor tag. Absent or any other value reads as "not
/// explicitly false".
fn attr_is_false(t: &Tag, key: &str) -> bool {
    match t.attributes.get(key) {
        Some(Scalar::Boolean(b)) => !b,
        Some(Scalar::String(s)) => {
            let s = s.trim();
            s.eq_ignore_ascii_case("false") || s == "0"
        }
        _ => false,
    }
}

fn layout_heading(tag: &Tag, level: u8, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let h = ctx.style.heading.for_level(level).clone();
    let column_span = ctx.heading_spans_columns(level);
    let lay_w = if column_span { ctx.body_full_width() } else { width };

    // Pull anchor declarations (`{% tag id="X" %}`) out of the heading's
    // children so the id is recorded on the resulting block. Returns
    // the first id found; subsequent declarations on the same heading
    // are ignored (uncommon, but tolerated). The same scan reads an
    // optional `numbered="false"` attribute which opts the heading out
    // of automatic section numbering (used for front-matter headings).
    let mut heading_anchor: Option<String> = None;
    let mut opt_out_numbering = false;
    for child in &tag.children {
        if let RenderableTreeNode::Tag(t) = child
            && t.name == "tag"
        {
            if heading_anchor.is_none()
                && let Some(id) = super::inline::anchor_id_attr(t)
            {
                heading_anchor = Some(id);
            }
            if attr_is_false(t, "numbered") {
                opt_out_numbering = true;
            }
        }
    }

    let mut text = String::new();
    let mut ranges = Vec::new();
    collect_inlines(&mut text, &mut ranges, &tag.children, &mut ctx.footnotes);
    if text.trim().is_empty() {
        return Vec::new();
    }

    // Prepend the automatic section number when enabled and not opted
    // out. Baking it into `text` (and the outline text below) means it
    // flows to the visible heading, the running header, and the ToC
    // without any further plumbing — each consumes the heading string.
    if !opt_out_numbering && let Some(number) = ctx.bump_heading_number(level) {
        let prefix = format!("{number}{}", ctx.style.heading_numbering.separator);
        // Shift any inline style ranges right by the prefix length so
        // bold/italic spans keep covering the original words.
        let shift = prefix.len();
        for r in &mut ranges {
            r.start += shift;
            r.end += shift;
        }
        text.insert_str(0, &prefix);
    }

    // Emit space-before as an empty (zero-height) spacer block so the
    // paginator sees it; otherwise consecutive headings on a fresh page
    // would push the heading off the top edge.
    let mut blocks = Vec::new();
    if h.space_before > 0.0 {
        blocks.push(spacer_block(h.space_before));
    }

    let style = TextStyle {
        font_size: h.font_size,
        font_weight: h.font_weight,
        line_height: ctx.style.body_line_height,
        color: h.color.unwrap_or(ctx.style.text_color).into(),
        font_families: ctx.body_families,
        italic: false,
    };
    let layout = build_layout(&text, &ranges, &style, lay_w, ctx.font_cx, ctx.layout_cx);
    let outline_text = text.clone();
    let slice = TextSlice::whole(layout, text, Vec::new(), x);
    let height = slice.height();
    // Auto-assign a synthetic anchor when the heading didn't have an
    // explicit `{% tag %}` declaration. ToC entries (and any future
    // "back to chapter" links) need every heading to be linkable.
    let heading_id = ctx.next_heading_id();
    let resolved_anchor = heading_anchor.unwrap_or_else(|| format!("__heading_{heading_id}"));

    blocks.push(Block {
        height,
        space_after: h.space_after,
        draw: BlockDraw::Text(slice),
        outline: Some(OutlineEntry {
            level,
            text: outline_text,
        }),
        anchor_id: Some(resolved_anchor),

        tag_role: None,
        page_column: 0,
        column_span,
    });
    blocks
}

// ── Lists ───────────────────────────────────────────────────────────────

#[derive(Copy, Clone)]
enum ListKind {
    Unordered,
    Ordered,
}

/// `{% list type="…" %}` wraps a `ul`/`ol`; render it with a custom marker.
/// An unknown or absent `type` falls back to the normal bullet / number.
fn layout_list_tag(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let marker = list_marker_glyph(tag);
    let inner = tag.children.iter().find_map(|c| match c {
        RenderableTreeNode::Tag(t) if t.name == "ul" => Some((t, ListKind::Unordered)),
        RenderableTreeNode::Tag(t) if t.name == "ol" => Some((t, ListKind::Ordered)),
        _ => None,
    });
    match inner {
        Some((ul, kind)) => layout_list(ul, kind, marker, x, width, ctx),
        None => layout_children(&tag.children, x, width, ctx),
    }
}

/// The marker glyph for a `{% list type=… %}`: `checkmark` → `✓`, `dash` →
/// `–`, `none` → no marker. Anything else (or absent) → `None` (default list).
fn list_marker_glyph(tag: &Tag) -> Option<&'static str> {
    match tag.attributes.get("type") {
        Some(Scalar::String(s)) => match s.as_str() {
            "checkmark" => Some("✓"),
            "dash" => Some("–"),
            "none" => Some(""),
            _ => None,
        },
        _ => None,
    }
}

fn layout_list(
    tag: &Tag,
    kind: ListKind,
    marker: Option<&str>,
    x: f32,
    width: f32,
    ctx: &mut LayoutCtx<'_>,
) -> Vec<Block> {
    let indent = ctx.style.list_indent;
    let marker_x = x;

    // Iterate <li> children; non-li children (rare) are skipped.
    let items: Vec<&RenderableTreeNode> = tag
        .children
        .iter()
        .filter(|c| matches!(c, RenderableTreeNode::Tag(t) if t.name == "li"))
        .collect();

    let mut out = Vec::new();
    for (idx, item_node) in items.iter().enumerate() {
        let RenderableTreeNode::Tag(item_tag) = item_node else {
            continue;
        };
        let ordered = matches!(kind, ListKind::Ordered);
        // A `{% list type=… %}` marker override (e.g. a checkmark) replaces
        // the bullet/number and is never drawn as a circle badge.
        let badge_on = marker.is_none() && ordered && ctx.style.list_marker.badge;
        let marker_text = match (marker, kind) {
            (Some(m), _) => m.to_string(),
            (None, ListKind::Unordered) => "•".to_string(),
            (None, ListKind::Ordered) => {
                let seq = ctx.style.list_marker.sequence_for_depth(ctx.list_depth);
                let label = format_ordered_marker(idx + 1, seq);
                // A badge delimits the marker visually, so the trailing
                // dot is dropped in that mode.
                if badge_on { label } else { format!("{label}.") }
            }
        };
        // Inside a badge the marker uses the badge text colour; otherwise
        // it inherits the body text colour.
        let marker_color = if badge_on {
            ctx.style
                .list_marker
                .badge_text_color
                .unwrap_or(ctx.style.text_color)
                .into()
        } else {
            ctx.style.text_color.into()
        };
        // A `{% list type="checkmark" %}` marker is drawn as a vector (the
        // `✓` glyph is widely missing) in the marker colour.
        let check = (marker == Some("✓")).then_some(marker_color);
        let marker_style = TextStyle {
            font_size: ctx.style.body_font_size,
            font_weight: 400.0,
            line_height: ctx.style.body_line_height,
            color: marker_color,
            font_families: ctx.body_families,
            italic: false,
        };
        let marker_layout = build_layout(
            &marker_text,
            &[],
            &marker_style,
            indent,
            ctx.font_cx,
            ctx.layout_cx,
        );

        // Badge geometry: a circle sized off the body font, centred on
        // the first line's text middle (baseline less ~half cap-height)
        // so the marker — drawn at its natural y — lands centred in it.
        let badge = if badge_on {
            let fs = ctx.style.body_font_size;
            let baseline = marker_layout
                .lines()
                .next()
                .map(|l| l.metrics().baseline)
                .unwrap_or(fs);
            let diameter = fs * ctx.style.list_marker.badge_scale;
            Some(MarkerBadge {
                fill: ctx.style.list_marker.badge_fill.into(),
                diameter,
                center_dx: diameter * 0.5,
                center_dy: baseline - 0.32 * fs,
            })
        } else {
            None
        };

        // Where the item's text begins. A badge keeps the full indent as
        // its circular gutter; a plain bullet/number sits at the column's
        // left edge with just `list_marker_gap` before the text, so a
        // narrow marker never leaves a wide gap.
        let marker_w = marker_layout
            .lines()
            .next()
            .map(|l| l.metrics().advance)
            .unwrap_or(0.0);
        let text_indent = if badge_on {
            indent
        } else {
            marker_w + ctx.style.list_marker_gap
        };
        let content_x = x + text_indent;
        let content_w = (width - text_indent).max(0.0);

        // Lay out the item body one nesting level deeper so a nested
        // ordered list picks the next numbering style. <li> children may
        // be paragraphs, nested lists, etc. — recurse normally.
        ctx.list_depth += 1;
        let body = layout_li_body(item_tag, content_x, content_w, ctx);
        ctx.list_depth -= 1;
        let body_height: f32 = body
            .iter()
            .map(|b| b.height + b.space_after)
            .sum::<f32>()
            // Trim trailing space_after of the last child — the item's
            // own list_item_space_after takes its place.
            .max(0.0);
        let body_height = body_height - body.last().map(|b| b.space_after).unwrap_or(0.0);

        out.push(Block {
            height: body_height.max(marker_layout.height()),
            space_after: ctx.style.list_item_space_after,
            draw: BlockDraw::ListItem {
                marker: marker_layout,
                marker_text,
                marker_x,
                body,
                ordered,
                badge,
                check,
            },
            outline: None,
            anchor_id: None,

            tag_role: None,
            page_column: 0,
            column_span: false,
        });
    }

    // No extra space after the whole list (the last item carries its own
    // space_after; behaves consistently with other top-level blocks).
    out
}

/// Format a 1-based ordered-list position in the given sequence style.
fn format_ordered_marker(n: usize, seq: MarkerSequence) -> String {
    match seq {
        MarkerSequence::Decimal => n.to_string(),
        MarkerSequence::LowerAlpha => to_alpha(n, false),
        MarkerSequence::UpperAlpha => to_alpha(n, true),
        MarkerSequence::LowerRoman => to_roman(n, false),
        MarkerSequence::UpperRoman => to_roman(n, true),
    }
}

/// Spreadsheet-style bijective base-26: 1→a, 26→z, 27→aa, 28→ab, …
fn to_alpha(mut n: usize, upper: bool) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let base = if upper { b'A' } else { b'a' };
    let mut buf = Vec::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        buf.push(base + rem as u8);
        n = (n - 1) / 26;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

/// Roman numerals for 1..=3999; anything outside that classic range
/// falls back to decimal.
fn to_roman(mut n: usize, upper: bool) -> String {
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    const VALS: [(usize, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut s = String::new();
    for (v, sym) in VALS {
        while n >= v {
            s.push_str(sym);
            n -= v;
        }
    }
    if upper { s.to_uppercase() } else { s }
}

/// Layout the contents of one `<li>`. Markdown list items often wrap
/// their contents in a paragraph for "loose" lists or place text
/// directly for "tight" lists; both cases reduce to "lay out children".
fn layout_li_body(item: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    // If the <li>'s direct children are all inline (text/strong/em/...) and
    // there's no block-level child, treat the whole thing as one paragraph.
    let any_block_child = item.children.iter().any(|c| {
        if let RenderableTreeNode::Tag(t) = c {
            matches!(
                t.name.as_str(),
                "p" | "ul"
                    | "ol"
                    | "blockquote"
                    | "pre"
                    | "callout"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "hr"
            )
        } else {
            false
        }
    });
    if !any_block_child {
        // Synthesize a paragraph wrapper to reuse layout_paragraph.
        return layout_paragraph(item, x, width, ctx);
    }
    // Mixed item: leading text (e.g. "A screwdriver, with the bits:")
    // followed by a block child (a nested list). markdoc leaves that
    // leading text as bare inline nodes, which `layout_children` would
    // drop — wrap each inline run in a synthetic paragraph first so the
    // item's own text renders above its nested list.
    let wrapped = wrap_inline_runs_in_paragraphs(&item.children);
    layout_children(&wrapped, x, width, ctx)
}

// ── Block quote ─────────────────────────────────────────────────────────

fn layout_blockquote(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let indent = ctx.style.blockquote_indent;
    let inner_x = x + indent;
    let inner_w = width - indent;

    // Lay out children with shifted text colour to set them apart visually.
    // We do this by walking children manually with a tweaked TextStyle —
    // but for simplicity we just use the regular layout and rely on the
    // accent bar drawn by the BoxedGroup decoration.
    let children = layout_children(&tag.children, inner_x, inner_w, ctx);
    let inner_height: f32 = children
        .iter()
        .map(|b| b.height + b.space_after)
        .sum::<f32>()
        - children.last().map(|b| b.space_after).unwrap_or(0.0);

    vec![Block {
        height: inner_height,
        space_after: ctx.style.paragraph_space_after,
        draw: BlockDraw::BoxedGroup {
            x,
            width,
            background: None,
            border: None,
            accent_left: Some(ctx.style.blockquote_bar_color.into()),
            accent_width: ctx.style.blockquote_bar_width,
            padding: 0.0, // children already shifted via inner_x
            children,
            icon: None,
            top_rule: None,
            bottom_rule: None,
        },
        outline: None,
        anchor_id: None,

        tag_role: None,
        page_column: 0,
        column_span: false,
    }]
}

// ── Code block ──────────────────────────────────────────────────────────

fn layout_code_block(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    // <pre> in our transformer typically wraps a Code node with the source
    // as its content scalar. Extract the raw text.
    let source = extract_code_text(&tag.children);
    if source.trim().is_empty() {
        return Vec::new();
    }

    let padding = ctx.style.code_padding;
    let inner_x = x + padding;
    let inner_w = width - 2.0 * padding;

    let mono_families = monospace_families(&ctx.style.code_font_family);
    // SAFETY: monospace_families returns &'static str entries.
    let leaked: Vec<&'static str> = mono_families.into_iter().collect();
    let style = TextStyle {
        font_size: ctx.style.code_font_size,
        font_weight: 400.0,
        line_height: ctx.style.body_line_height,
        color: ctx.style.code_text_color.into(),
        font_families: Box::leak(leaked.into_boxed_slice()),
        italic: false,
    };

    // Highlight: schema renders the fence's `language` attribute as
    // `data-language`. Map every token to an InlineRange::Color tinted
    // by the style palette. Unknown languages produce no spans, so the
    // block renders in plain `code_text_color`.
    let lang = match tag.attributes.get("data-language") {
        Some(Scalar::String(s)) => s.as_str(),
        _ => "",
    };
    let palette = &ctx.style.code_highlight;
    let ranges: Vec<super::inline::InlineRange> = if lang.is_empty() {
        Vec::new()
    } else {
        super::highlight::tokenize(lang, &source)
            .into_iter()
            .map(|t| {
                let color: krilla::color::rgb::Color = match t.class {
                    super::highlight::TokenClass::Keyword => palette.keyword.into(),
                    super::highlight::TokenClass::String => palette.string.into(),
                    super::highlight::TokenClass::Comment => palette.comment.into(),
                    super::highlight::TokenClass::Number => palette.number.into(),
                };
                super::inline::InlineRange {
                    start: t.start,
                    end: t.end,
                    prop: super::inline::InlineProp::Color(color),
                }
            })
            .collect()
    };

    let layout = build_layout(
        &source,
        &ranges,
        &style,
        inner_w,
        ctx.font_cx,
        ctx.layout_cx,
    );
    let inner_height = layout.height();

    let slice = TextSlice::whole(layout, source, Vec::new(), inner_x);
    let inner_block = Block {
        height: inner_height,
        space_after: 0.0,
        draw: BlockDraw::Text(slice),
        outline: None,
        anchor_id: None,

        tag_role: None,
        page_column: 0,
        column_span: false,
    };

    vec![Block {
        height: inner_height + 2.0 * padding,
        space_after: ctx.style.paragraph_space_after,
        draw: BlockDraw::BoxedGroup {
            x,
            width,
            background: Some(ctx.style.code_background.into()),
            border: None,
            accent_left: None,
            accent_width: 0.0,
            padding,
            children: vec![inner_block],
            icon: None,
            top_rule: None,
            bottom_rule: None,
        },
        outline: None,
        anchor_id: None,

        tag_role: None,
        page_column: 0,
        column_span: false,
    }]
}

fn extract_code_text(children: &[RenderableTreeNode]) -> String {
    let mut s = String::new();
    for child in children {
        match child {
            RenderableTreeNode::Scalar(Scalar::String(text)) => s.push_str(text),
            RenderableTreeNode::Tag(t) => s.push_str(&extract_code_text(&t.children)),
            _ => {}
        }
    }
    s
}

/// True for tag names that are laid out as block-level constructs.
/// Anything not in this set (plain `Scalar`s, `strong`, `em`, `a`, …)
/// is treated as inline content and grouped into a paragraph wrapper
/// by [`wrap_inline_runs_in_paragraphs`].
fn is_block_tag(name: &str) -> bool {
    matches!(
        name,
        "p" | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "blockquote"
            | "pre"
            | "callout"
            | "table"
            | "hr"
            | "div"
            | "img"
            | "media"
    )
}

/// Group consecutive inline children (bare `Scalar`s, inline `Tag`s like
/// `strong`/`em`/`a`/`code`) into synthetic `<p>` wrappers, leaving
/// block-level children untouched. Needed because markdoc's parser
/// leaves loose inline content directly under a block-level tag like
/// `{% callout %}…{% /callout %}` — without this wrapping
/// `layout_node` would drop the bare scalars.
fn wrap_inline_runs_in_paragraphs(children: &[RenderableTreeNode]) -> Vec<RenderableTreeNode> {
    let mut out: Vec<RenderableTreeNode> = Vec::new();
    let mut run: Vec<RenderableTreeNode> = Vec::new();
    let flush = |run: &mut Vec<RenderableTreeNode>, out: &mut Vec<RenderableTreeNode>| {
        if run.is_empty() {
            return;
        }
        let p = Tag {
            name: "p".to_string(),
            attributes: std::collections::HashMap::new(),
            children: std::mem::take(run),
        };
        out.push(RenderableTreeNode::Tag(Box::new(p)));
    };
    for child in children {
        match child {
            RenderableTreeNode::Tag(t) if is_block_tag(&t.name) => {
                flush(&mut run, &mut out);
                out.push(child.clone());
            }
            _ => run.push(child.clone()),
        }
    }
    flush(&mut run, &mut out);
    out
}

// ── Callout ─────────────────────────────────────────────────────────────

fn layout_callout(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let kind = match tag.attributes.get("type") {
        Some(Scalar::String(s)) => s.as_str(),
        _ => "note",
    };
    let cs = ctx.style.callout_styles.for_kind(kind).clone();
    let padding = ctx.style.callout_padding;
    // The left accent stripe only exists in the box framing; the bulletin
    // (rules) framing has none, so it reserves no left gutter.
    let accent_w = match cs.decoration {
        super::style::CalloutDecoration::Box => ctx.style.callout_accent_width,
        super::style::CalloutDecoration::Rules => 0.0,
    };
    // Inner column accounts for both the accent bar (on the left) and the
    // padding on both sides.
    let inner_x = x + accent_w + padding;
    let inner_w = width - accent_w - 2.0 * padding;

    // Decode the optional icon once (cached by the resolver). When
    // present, the label and body are indented past an icon gutter so
    // they sit beside it rather than under it.
    let icon = if cs.icon.trim().is_empty() {
        None
    } else {
        super::decoration::decode_media(cs.icon.trim(), ctx.assets).map(|decoded| BoxedGroupIcon {
            decoded,
            x: inner_x,
            size: ctx.style.callout_icon_size,
        })
    };
    let is_rules = matches!(cs.decoration, super::style::CalloutDecoration::Rules);
    let icon_size = icon.as_ref().map(|i| i.size).unwrap_or(0.0);
    // The box framing indents the label + body past an icon gutter so they
    // sit beside the icon. The rules framing instead flows the body the
    // full width beneath the icon, aligned with the icon's left edge.
    let icon_gutter = if is_rules {
        0.0
    } else {
        icon.as_ref()
            .map(|i| i.size + ctx.style.callout_icon_gap)
            .unwrap_or(0.0)
    };
    let content_x = inner_x + icon_gutter;
    let content_w = inner_w - icon_gutter;

    // Optional bold label as the first child line.
    let mut children: Vec<Block> = Vec::new();
    if !cs.label.trim().is_empty() {
        let mut label_block = build_callout_label_block(
            cs.label.trim(),
            content_x,
            content_w,
            cs.label_color.unwrap_or(ctx.style.text_color).into(),
            cs.label_centered,
            ctx,
        );
        // In the rules framing the body flows beneath the icon, so the
        // label row reserves the icon's height to push the body clear.
        if is_rules && icon_size > label_block.height {
            label_block.height = icon_size;
        }
        children.push(label_block);
    } else if is_rules && icon_size > 0.0 {
        // No label: reserve the icon's height before the body so it
        // doesn't render on top of the icon.
        children.push(spacer_block(icon_size));
    }

    // markdoc's parser doesn't wrap a callout's text content in a `<p>` —
    // body children come through as raw `Scalar`s and inline tags. Group
    // consecutive inline children into synthetic paragraphs so they
    // actually render; block-level tags (lists, tables, nested callouts)
    // are still laid out individually.
    let wrapped = wrap_inline_runs_in_paragraphs(&tag.children);
    children.extend(layout_children(&wrapped, content_x, content_w, ctx));
    let content_height: f32 = children
        .iter()
        .map(|b| b.height + b.space_after)
        .sum::<f32>()
        - children.last().map(|b| b.space_after).unwrap_or(0.0);
    // The box must clear both the content and (if taller) the icon.
    let inner_height = content_height.max(icon.as_ref().map(|i| i.size).unwrap_or(0.0));

    // Two framings: a filled box (default) or a pair of horizontal rules
    // (bulletin style). The rule colour is the accent.
    let (background, border, accent_left, top_rule, bottom_rule) = match cs.decoration {
        super::style::CalloutDecoration::Box => (
            Some(cs.background.into()),
            Some(cs.border.into()),
            Some(cs.accent.into()),
            None,
            None,
        ),
        super::style::CalloutDecoration::Rules => {
            let rule = (cs.accent.into(), ctx.style.callout_rule_thickness);
            (None, None, None, Some(rule), Some(rule))
        }
    };

    vec![Block {
        height: inner_height + 2.0 * padding,
        space_after: ctx.style.callout_space_after,
        draw: BlockDraw::BoxedGroup {
            x,
            width,
            background,
            border,
            accent_left,
            accent_width: accent_w,
            padding,
            children,
            icon,
            top_rule,
            bottom_rule,
        },
        outline: None,
        anchor_id: None,

        tag_role: None,
        page_column: 0,
        column_span: false,
    }]
}

/// Build the bold single-line label block (e.g. `WARNING`) that heads a
/// callout. Positioned at `x` with the given colour; `centered` spreads
/// it across `width` instead of left-aligning.
fn build_callout_label_block(
    label: &str,
    x: f32,
    width: f32,
    color: rgb::Color,
    centered: bool,
    ctx: &mut LayoutCtx<'_>,
) -> Block {
    let style = TextStyle {
        font_size: ctx.style.callout_label_size,
        font_weight: 700.0,
        line_height: ctx.style.body_line_height,
        color,
        font_families: ctx.body_families,
        italic: false,
    };
    let layout = if centered {
        super::text::build_layout_aligned(
            label,
            &[],
            &style,
            width,
            parley::layout::Alignment::Center,
            ctx.font_cx,
            ctx.layout_cx,
        )
    } else {
        build_layout(label, &[], &style, width, ctx.font_cx, ctx.layout_cx)
    };
    let slice = TextSlice::whole(layout, label.to_string(), Vec::new(), x);
    let height = slice.height();
    Block {
        height,
        space_after: ctx.style.callout_label_size * 0.4,
        draw: BlockDraw::Text(slice),
        outline: None,
        anchor_id: None,
        tag_role: None,
        page_column: 0,
        column_span: false,
    }
}

// ── Horizontal rule ─────────────────────────────────────────────────────

fn layout_rule(x: f32, width: f32, style: &Style) -> Block {
    Block {
        height: style.rule_thickness,
        space_after: style.rule_space_around,
        draw: BlockDraw::Rule {
            x,
            width,
            thickness: style.rule_thickness,
            color: style.rule_color.into(),
        },
        outline: None,
        anchor_id: None,

        tag_role: None,
        page_column: 0,
        column_span: false,
    }
}

/// Sentinel anchor id stamped on the `{% toc /%}` marker block. The
/// leading control character keeps it from ever colliding with an
/// author-declared `{% tag id="…" %}` anchor.
pub const TOC_MARKER_ANCHOR: &str = "\u{0}toc-marker";

/// Zero-height marker block for `{% toc /%}`. Renders nothing; the
/// renderer finds it by [`TOC_MARKER_ANCHOR`] to place the ToC.
fn toc_marker_block(x: f32) -> Block {
    Block {
        height: 0.0,
        space_after: 0.0,
        draw: BlockDraw::Rule {
            x,
            width: 0.0,
            thickness: 0.0,
            color: rgb::Color::new(0, 0, 0),
        },
        outline: None,
        anchor_id: Some(TOC_MARKER_ANCHOR.to_string()),
        tag_role: None,
        page_column: 0,
        column_span: false,
    }
}

/// A page-break marker block (flushes the current page during pagination).
fn page_break_block() -> Block {
    Block {
        height: 0.0,
        space_after: 0.0,
        draw: BlockDraw::PageBreak,
        outline: None,
        anchor_id: None,
        tag_role: None,
        page_column: 0,
        column_span: false,
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn spacer_block(height: f32) -> Block {
    // Use a degenerate Rule with zero thickness to represent vertical
    // whitespace; the emit pass treats zero-thickness rules as no-ops.
    Block {
        height,
        space_after: 0.0,
        draw: BlockDraw::Rule {
            x: 0.0,
            width: 0.0,
            thickness: 0.0,
            color: rgb::Color::new(0, 0, 0),
        },
        outline: None,
        anchor_id: None,

        tag_role: None,
        page_column: 0,
        column_span: false,
    }
}

// ── Tables ──────────────────────────────────────────────────────────────

/// Lay out a `<table>` into a single `Table` block. Equal column widths;
/// header rows get bold text + tinted background; cells can hold any
/// block content (paragraphs, lists, …).
/// Per-table style overrides parsed from a `{% table … %}` tag's
/// attributes. Every field is optional — `None` means "inherit the
/// document-wide table style", so a plain pipe table (no attributes)
/// yields an all-`None` override and renders with the global style.
#[derive(Default)]
struct TableOverride {
    borders: Option<super::style::TableBorders>,
    border_color: Option<rgb::Color>,
    edge_color: Option<rgb::Color>,
    header_bg: Option<rgb::Color>,
    cell_padding: Option<f32>,
    column_weights: Option<Vec<f32>>,
    /// `Some(Some(c))` = stripe with colour `c`; `Some(None)` = explicitly
    /// off (`stripe="none"`); `None` = inherit the document default.
    stripe: Option<Option<rgb::Color>>,
    header_column: Option<bool>,
}

impl TableOverride {
    fn from_attrs(attrs: &std::collections::HashMap<String, Scalar>) -> Self {
        use super::inline::parse_css_color;
        use super::style::TableBorders;
        let color = |key: &str| match attrs.get(key) {
            Some(Scalar::String(s)) => parse_css_color(s),
            _ => None,
        };
        let borders = match attrs.get("borders") {
            Some(Scalar::String(s)) => match s.as_str() {
                "grid" => Some(TableBorders::Grid),
                "horizontal" => Some(TableBorders::Horizontal),
                "none" => Some(TableBorders::None),
                _ => None,
            },
            _ => None,
        };
        let cell_padding = match attrs.get("cell_padding") {
            Some(Scalar::Number(n)) => Some(*n as f32),
            Some(Scalar::String(s)) => s.trim().parse().ok(),
            _ => None,
        };
        // Weights are a space/comma-separated string because the tag parser
        // doesn't accept literal arrays (e.g. `column_weights="1 3.5"`).
        let column_weights = match attrs.get("column_weights") {
            Some(Scalar::String(s)) => {
                let ws: Vec<f32> = s
                    .split([',', ' '])
                    .filter(|t| !t.is_empty())
                    .filter_map(|t| t.parse().ok())
                    .collect();
                (!ws.is_empty()).then_some(ws)
            }
            _ => None,
        };
        let stripe = match attrs.get("stripe") {
            Some(Scalar::String(s)) if s.eq_ignore_ascii_case("none") => Some(None),
            Some(Scalar::String(s)) => parse_css_color(s).map(Some),
            _ => None,
        };
        let header_column = match attrs.get("header_column") {
            Some(Scalar::Boolean(b)) => Some(*b),
            Some(Scalar::String(s)) => Some(s.eq_ignore_ascii_case("true")),
            _ => None,
        };
        TableOverride {
            borders,
            border_color: color("border_color"),
            edge_color: color("edge_color"),
            header_bg: color("header_background"),
            cell_padding,
            column_weights,
            stripe,
            header_column,
        }
    }
}

/// Find the node that actually holds the table rows. A pipe table is a
/// `Tag("table")` whose children are `<thead>`/`<tbody>`/`<tr>`. When the
/// author wraps a pipe table in `{% table … %}` to style it, the real
/// `<table>` sits one level down as a child, so descend into it.
fn table_rows_source(tag: &Tag) -> &Tag {
    let has_rows = tag.children.iter().any(|c| {
        matches!(c, RenderableTreeNode::Tag(t)
            if matches!(t.name.as_str(), "thead" | "tbody" | "tr"))
    });
    if has_rows {
        return tag;
    }
    for c in &tag.children {
        if let RenderableTreeNode::Tag(t) = c
            && t.name == "table"
        {
            return t;
        }
    }
    tag
}

/// Inner padding of a `{% columns/grid background=… %}` panel, in points.
const PANEL_PAD: f32 = 10.0;

/// Split a `{% columns %}` / `{% grid %}` tag's children into cells. Prefer a
/// markdown list (one `li` per cell, each free to hold several blocks —
/// e.g. an image plus a headline plus body); otherwise each block-level
/// child of the tag is its own cell.
fn columns_from_children(tag: &Tag) -> Vec<Vec<RenderableTreeNode>> {
    let list_children = tag.children.iter().find_map(|c| match c {
        RenderableTreeNode::Tag(t) if matches!(t.name.as_str(), "ul" | "ol") => {
            Some(t.children.as_slice())
        }
        _ => None,
    });
    match list_children {
        Some(items) => items
            .iter()
            .filter_map(|li| match li {
                RenderableTreeNode::Tag(t) if t.name == "li" => Some(t.children.clone()),
                _ => None,
            })
            .collect(),
        None => tag
            .children
            .iter()
            .filter(|c| matches!(c, RenderableTreeNode::Tag(_)))
            .map(|c| vec![c.clone()])
            .collect(),
    }
}

/// A points value from a layout tag's numeric-or-string attribute.
fn attr_points(attrs: &std::collections::HashMap<String, Scalar>, key: &str, default: f32) -> f32 {
    attrs
        .get(key)
        .and_then(|s| match s {
            Scalar::Number(v) => Some(*v as f32),
            Scalar::String(s) => s.trim().parse::<f32>().ok(),
            _ => None,
        })
        .unwrap_or(default)
        .max(0.0)
}

/// Relative column widths from a `widths="2 1"` / `widths=[2, 1]` attribute.
fn parse_widths(attrs: &std::collections::HashMap<String, Scalar>) -> Option<Vec<f32>> {
    match attrs.get("widths") {
        Some(Scalar::Array(items)) => Some(
            items
                .iter()
                .filter_map(|it| match it {
                    Scalar::Number(v) => Some(*v as f32),
                    _ => None,
                })
                .collect(),
        ),
        Some(Scalar::String(s)) => Some(
            s.split(|c: char| c.is_whitespace() || c == ',')
                .filter(|t| !t.is_empty())
                .filter_map(|t| t.parse::<f32>().ok())
                .collect(),
        ),
        _ => None,
    }
}

/// `align="left|center|right"` on a layout tag → parley alignment.
fn parse_layout_align(
    attrs: &std::collections::HashMap<String, Scalar>,
) -> Option<parley::layout::Alignment> {
    use parley::layout::Alignment;
    match attrs.get("align") {
        Some(Scalar::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "center" | "centre" => Some(Alignment::Center),
            "right" | "end" => Some(Alignment::End),
            "left" | "start" => Some(Alignment::Start),
            _ => None,
        },
        _ => None,
    }
}

/// `background="#f9f9f9"` on a layout tag → panel fill colour.
fn parse_layout_background(
    attrs: &std::collections::HashMap<String, Scalar>,
) -> Option<rgb::Color> {
    match attrs.get("background") {
        Some(Scalar::String(s)) => super::inline::parse_css_color(s),
        _ => None,
    }
}

/// Lay a single row of `cells` out as a borderless, weighted table — the
/// shared engine behind `{% columns %}` (one row) and `{% grid %}` (each
/// wrapped row). `weights` sets relative column widths when its length
/// matches the cell count, else equal. `gap` (points) becomes cell padding
/// `gap/2` so adjacent cells sit `gap` apart. `align` centres / right-aligns
/// every cell's content — routed both through the table's per-column `align`
/// array (for simple one-paragraph cells) and the `cell_content_align`
/// context flag (which reaches the multi-block image+headline+body cells the
/// column mechanism alone misses).
fn layout_cells_row(
    cells: Vec<Vec<RenderableTreeNode>>,
    x: f32,
    width: f32,
    gap: f32,
    weights: Option<&[f32]>,
    align: Option<parley::layout::Alignment>,
    ctx: &mut LayoutCtx<'_>,
) -> Vec<Block> {
    use parley::layout::Alignment;
    let n = cells.len();
    let tds: Vec<RenderableTreeNode> = cells
        .into_iter()
        .map(|blocks| {
            RenderableTreeNode::Tag(Box::new(Tag {
                name: "td".to_string(),
                attributes: std::collections::HashMap::new(),
                children: blocks,
            }))
        })
        .collect();
    let tr = RenderableTreeNode::Tag(Box::new(Tag {
        name: "tr".to_string(),
        attributes: std::collections::HashMap::new(),
        children: tds,
    }));
    let weights_str = weights
        .filter(|w| w.len() == n && w.iter().all(|v| *v > 0.0))
        .map(|w| {
            w.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| vec!["1"; n].join(" "));

    let mut table_attrs = std::collections::HashMap::new();
    table_attrs.insert("borders".to_string(), Scalar::String("none".to_string()));
    table_attrs.insert("column_weights".to_string(), Scalar::String(weights_str));
    table_attrs.insert(
        "cell_padding".to_string(),
        Scalar::Number((gap / 2.0) as f64),
    );
    if let Some(a) = align {
        let s = match a {
            Alignment::Center => "center",
            Alignment::End => "right",
            _ => "left",
        };
        table_attrs.insert(
            "align".to_string(),
            Scalar::Array(vec![Scalar::String(s.to_string()); n]),
        );
    }
    let table = Tag {
        name: "table".to_string(),
        attributes: table_attrs,
        children: vec![tr],
    };
    let prev = ctx.cell_content_align;
    ctx.cell_content_align = align;
    let blocks = layout_table(&table, x, width, ctx);
    ctx.cell_content_align = prev;
    blocks
}

/// Wrap already-laid-out `inner` blocks in a filled background panel — the
/// `background=` look for columns / grids. `inner` must already be positioned
/// at `x + PANEL_PAD` with the inset width; the panel spans the full `width`.
/// Being one `BoxedGroup`, the panel is an indivisible pagination unit.
fn wrap_in_panel(inner: Vec<Block>, x: f32, width: f32, bg: rgb::Color, space_after: f32) -> Block {
    let content_height: f32 = inner.iter().map(|b| b.height + b.space_after).sum::<f32>()
        - inner.last().map(|b| b.space_after).unwrap_or(0.0);
    Block {
        height: content_height + 2.0 * PANEL_PAD,
        space_after,
        draw: BlockDraw::BoxedGroup {
            x,
            width,
            background: Some(bg),
            border: None,
            accent_left: None,
            accent_width: 0.0,
            padding: PANEL_PAD,
            children: inner,
            icon: None,
            top_rule: None,
            bottom_rule: None,
        },
        outline: None,
        anchor_id: None,
        tag_role: None,
        page_column: 0,
        column_span: false,
    }
}

/// `{% columns %}` — lay children out in equal-width columns, side by side.
///
/// Two authoring forms:
///   - a markdown list, one item per column (each item may hold several
///     blocks — e.g. an image plus a headline plus body);
///   - or blank-line-separated blocks, one block per column.
///
/// Implemented as a borderless single-row table so it reuses the whole table
/// path (column widths, row height, pagination, image cells). `gap` (points,
/// default 16) sets the space between columns; `widths="2 1"` makes them
/// uneven; `align="center"` centres each cell's content; `background="#…"`
/// paints a panel behind the row.
fn layout_columns(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let columns = columns_from_children(tag);
    if columns.is_empty() {
        return layout_children(&tag.children, x, width, ctx);
    }
    let gap = attr_points(&tag.attributes, "gap", 16.0);
    let align = parse_layout_align(&tag.attributes);
    let bg = parse_layout_background(&tag.attributes);
    let widths = parse_widths(&tag.attributes);

    let (inner_x, inner_w) = match bg {
        Some(_) => (x + PANEL_PAD, width - 2.0 * PANEL_PAD),
        None => (x, width),
    };
    let rows = layout_cells_row(
        columns,
        inner_x,
        inner_w,
        gap,
        widths.as_deref(),
        align,
        ctx,
    );
    match bg {
        Some(c) => vec![wrap_in_panel(
            rows,
            x,
            width,
            c,
            ctx.style.paragraph_space_after,
        )],
        None => rows,
    }
}

/// Wrap `inner` in a transparent group (no fill / border / padding) purely
/// so the paginator treats it as one indivisible unit — a `BoxedGroup` falls
/// into `try_split`'s atomic branch, so a grid row moves whole to the next
/// page rather than splitting a figure from its caption mid-cell.
fn atomic_group(inner: Vec<Block>, x: f32, width: f32, space_after: f32) -> Block {
    let height: f32 = inner.iter().map(|b| b.height + b.space_after).sum::<f32>()
        - inner.last().map(|b| b.space_after).unwrap_or(0.0);
    Block {
        height,
        space_after,
        draw: BlockDraw::BoxedGroup {
            x,
            width,
            background: None,
            border: None,
            accent_left: None,
            accent_width: 0.0,
            padding: 0.0,
            children: inner,
            icon: None,
            top_rule: None,
            bottom_rule: None,
        },
        outline: None,
        anchor_id: None,
        tag_role: None,
        page_column: 0,
        column_span: false,
    }
}

/// `{% grid %}` — a responsive grid: cells reflow into as many equal columns
/// as fit at `min` points wide (default 120), wrapping into rows, exactly
/// like CSS `repeat(auto-fill, minmax(min, 1fr))`. `gap` (default 16) spaces
/// both columns and rows; `align` centres / right-aligns each cell's content;
/// `background="#…"` paints a panel behind the whole grid. Every row keeps
/// the same column count — a short final row is left-packed with its cells at
/// the same width as the rows above, so it doesn't stretch to fill.
fn layout_grid(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let cells = columns_from_children(tag);
    if cells.is_empty() {
        return layout_children(&tag.children, x, width, ctx);
    }
    let n = cells.len();
    let gap = attr_points(&tag.attributes, "gap", 16.0);
    let align = parse_layout_align(&tag.attributes);
    let bg = parse_layout_background(&tag.attributes);
    let min = attr_points(&tag.attributes, "min", 120.0).max(1.0);

    let (inner_x, inner_w) = match bg {
        Some(_) => (x + PANEL_PAD, width - 2.0 * PANEL_PAD),
        None => (x, width),
    };

    // How many `min`-wide columns fit with `gap` between them:
    //   cols·min + (cols−1)·gap ≤ inner_w  ⇒  cols ≤ (inner_w + gap)/(min + gap)
    let cols = (((inner_w + gap) / (min + gap)).floor() as usize).clamp(1, n);

    let chunks: Vec<Vec<Vec<RenderableTreeNode>>> =
        cells.chunks(cols).map(|c| c.to_vec()).collect();
    let n_rows = chunks.len();
    let mut out: Vec<Block> = Vec::new();
    for (i, mut row) in chunks.into_iter().enumerate() {
        // Pad the final short row with empty cells so its columns keep the
        // same width as the rows above (auto-fill, not auto-fit).
        while row.len() < cols {
            row.push(Vec::new());
        }
        let br = layout_cells_row(row, inner_x, inner_w, gap, None, align, ctx);
        // Each row is atomic (won't split mid-cell) with `gap` below it,
        // except the last. Rows still break between one another.
        let space_after = if i + 1 < n_rows { gap } else { 0.0 };
        out.push(atomic_group(br, inner_x, inner_w, space_after));
    }
    match bg {
        Some(c) => vec![wrap_in_panel(
            out,
            x,
            width,
            c,
            ctx.style.paragraph_space_after,
        )],
        None => out,
    }
}

/// A colour stop in a swatch gradient: a hex colour, an offset in `0..1`,
/// and an opacity (0 for a CSS `transparent` stop).
struct SwatchStop {
    color: String,
    offset: f32,
    opacity: f32,
}

/// How a `{% swatch %}` bar is filled.
enum SwatchFill {
    Solid(String),
    Gradient { angle: f32, stops: Vec<SwatchStop> },
}

/// Normalise a CSS colour (hex `#rgb` / `#rrggbb`, or a basic name) to a
/// `#rrggbb` string, so only sanitised hex reaches the synthesised SVG.
/// Mirrors `inline::parse_css_color`'s palette; `None` for `transparent` or
/// anything unrecognised.
fn css_hex(s: &str) -> Option<String> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix('#') {
        return match h.len() {
            3 if h.bytes().all(|b| b.is_ascii_hexdigit()) => {
                Some(h.chars().fold(String::from("#"), |mut a, c| {
                    a.push(c);
                    a.push(c);
                    a
                }))
            }
            6 if h.bytes().all(|b| b.is_ascii_hexdigit()) => {
                Some(format!("#{}", h.to_ascii_lowercase()))
            }
            _ => None,
        };
    }
    Some(
        match s.to_ascii_lowercase().as_str() {
            "red" => "#cc0000",
            "green" => "#008000",
            "blue" => "#0000cc",
            "orange" => "#ff7800",
            "black" => "#000000",
            "white" => "#ffffff",
            _ => return None,
        }
        .to_string(),
    )
}

/// CSS gradient angle (deg; 0 = up, 90 = right) → SVG objectBoundingBox
/// gradient axis `(x1, y1, x2, y2)` in `0..1` (y grows downward).
fn angle_to_axis(deg: f32) -> (f32, f32, f32, f32) {
    let r = deg.to_radians();
    let (dx, dy) = (r.sin(), -r.cos());
    (
        (0.5 - 0.5 * dx).clamp(0.0, 1.0),
        (0.5 - 0.5 * dy).clamp(0.0, 1.0),
        (0.5 + 0.5 * dx).clamp(0.0, 1.0),
        (0.5 + 0.5 * dy).clamp(0.0, 1.0),
    )
}

/// Parse a `gradient="[Ndeg,] stop, stop, …"` value: each stop is a CSS
/// colour (or `transparent`) with an optional `NN%` position; stops without
/// one are spread evenly. A `transparent` stop borrows its nearest opaque
/// neighbour's colour at zero opacity, so the fade stays in hue rather than
/// dipping through black.
fn parse_gradient(s: &str) -> Option<SwatchFill> {
    let mut parts: Vec<&str> = s
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    let mut angle = 90.0_f32;
    let first = parts[0];
    let is_stop = first.starts_with('#')
        || first.eq_ignore_ascii_case("transparent")
        || css_hex(first.split_whitespace().next().unwrap_or("")).is_some();
    if !is_stop {
        angle = first.trim_end_matches("deg").trim().parse::<f32>().ok()?;
        parts.remove(0);
    }
    let n = parts.len();
    if n < 2 {
        return None;
    }
    let mut colors: Vec<Option<String>> = Vec::with_capacity(n);
    let mut offsets: Vec<f32> = Vec::with_capacity(n);
    for (i, p) in parts.iter().enumerate() {
        let mut it = p.split_whitespace();
        let tok = it.next().unwrap_or("");
        let off = it
            .next()
            .and_then(|q| q.trim_end_matches('%').parse::<f32>().ok())
            .map(|v| (v / 100.0).clamp(0.0, 1.0))
            .unwrap_or(i as f32 / (n as f32 - 1.0));
        offsets.push(off);
        if tok.eq_ignore_ascii_case("transparent") {
            colors.push(None);
        } else {
            colors.push(Some(css_hex(tok)?)); // an invalid opaque colour fails the whole swatch
        }
    }
    let stops = (0..n)
        .map(|i| {
            if let Some(c) = &colors[i] {
                SwatchStop {
                    color: c.clone(),
                    offset: offsets[i],
                    opacity: 1.0,
                }
            } else {
                let near = (0..n)
                    .filter_map(|j| {
                        colors[j]
                            .as_ref()
                            .map(|c| ((j as i32 - i as i32).unsigned_abs(), c.clone()))
                    })
                    .min_by_key(|(d, _)| *d)
                    .map(|(_, c)| c)
                    .unwrap_or_else(|| "#ffffff".to_string());
                SwatchStop {
                    color: near,
                    offset: offsets[i],
                    opacity: 0.0,
                }
            }
        })
        .collect();
    Some(SwatchFill::Gradient { angle, stops })
}

/// The fill for a `{% swatch %}` from its attributes (`gradient` wins over
/// `color`).
fn parse_swatch_fill(attrs: &std::collections::HashMap<String, Scalar>) -> Option<SwatchFill> {
    if let Some(Scalar::String(g)) = attrs.get("gradient") {
        return parse_gradient(g);
    }
    if let Some(Scalar::String(c)) = attrs.get("color") {
        return css_hex(c).map(SwatchFill::Solid);
    }
    None
}

/// Build a self-contained SVG for a `w × h`-point swatch bar with corner
/// radius `radius`, filled solid or with a linear gradient.
fn swatch_svg(w: f32, h: f32, radius: f32, fill: &SwatchFill) -> String {
    let (defs, paint) = match fill {
        SwatchFill::Solid(hex) => (String::new(), format!("fill=\"{hex}\"")),
        SwatchFill::Gradient { angle, stops } => {
            let (x1, y1, x2, y2) = angle_to_axis(*angle);
            let els: String = stops
                .iter()
                .map(|s| {
                    format!(
                        "<stop offset=\"{:.4}\" stop-color=\"{}\" stop-opacity=\"{:.3}\"/>",
                        s.offset, s.color, s.opacity
                    )
                })
                .collect();
            (
                format!(
                    "<defs><linearGradient id=\"g\" x1=\"{x1:.4}\" y1=\"{y1:.4}\" x2=\"{x2:.4}\" y2=\"{y2:.4}\">{els}</linearGradient></defs>"
                ),
                "fill=\"url(#g)\"".to_string(),
            )
        }
    };
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w:.2}\" height=\"{h:.2}\" viewBox=\"0 0 {w:.2} {h:.2}\">{defs}<rect x=\"0\" y=\"0\" width=\"{w:.2}\" height=\"{h:.2}\" rx=\"{radius:.2}\" ry=\"{radius:.2}\" {paint}/></svg>"
    )
}

/// `{% swatch %}` — a block colour bar / chip, solid (`color="#…"`) or a
/// linear gradient (`gradient="90deg, #…, #…"`, evenly spaced stops unless a
/// `NN%` position is given; `transparent` allowed). Fills the available
/// width; `height` (points, default 6) and `radius` (default 2) size it. The
/// block analogue of the inline `{% color %}`, rendered as a synthesised SVG
/// so gradients come for free — handy for legends, status keys, indicator
/// bars.
fn layout_swatch(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let height = attr_points(&tag.attributes, "height", 6.0).max(0.5);
    let radius = attr_points(&tag.attributes, "radius", 2.0);
    let fill = match parse_swatch_fill(&tag.attributes) {
        Some(f) => f,
        None => return placeholder(x, width, ctx, "[swatch: needs color or gradient]"),
    };
    let svg = swatch_svg(width, height, radius, &fill);
    match usvg::Tree::from_data(svg.as_bytes(), &usvg::Options::default()) {
        Ok(tree) => vec![Block {
            height,
            space_after: 6.0,
            draw: BlockDraw::Svg {
                tree: Arc::new(tree),
                x,
                width,
                height,
                caption: None,
            },
            outline: None,
            anchor_id: None,
            tag_role: None,
            page_column: 0,
            column_span: false,
        }],
        Err(e) => {
            eprintln!("warning: swatch render failed — {e}");
            placeholder(x, width, ctx, "[swatch: render failed]")
        }
    }
}

/// One status-light entry — mirrors `components/LightIndicators.tsx` and
/// the `.light-indicator-bar.*` fills in `public/globals.css`.
struct LightIndicatorSpec {
    title: &'static str,
    description: &'static str,
    /// Solid CSS colour, or `None` when `gradient` is set.
    color: Option<&'static str>,
    /// Swatch `gradient="…"` string (CSS-compatible stops), or `None`.
    gradient: Option<&'static str>,
}

/// Status lights shown by `{% lightindicators /%}`. Colours / gradients
/// match the web component's CSS classes.
const LIGHT_INDICATORS: &[LightIndicatorSpec] = &[
    LightIndicatorSpec {
        title: "Red",
        description: "Emergency stop or Protective stop",
        color: Some("#ff0000"),
        gradient: None,
    },
    LightIndicatorSpec {
        title: "Green",
        description: "Ready for job",
        color: Some("#00fa00"),
        gradient: None,
    },
    LightIndicatorSpec {
        title: "Cyan",
        description: "Drives to destination",
        color: Some("#00fafa"),
        gradient: None,
    },
    LightIndicatorSpec {
        title: "Purple",
        description: "Goal/Path blocked",
        color: Some("#f900f9"),
        gradient: None,
    },
    LightIndicatorSpec {
        title: "Wavering white",
        description: "Planning path",
        color: None,
        // CSS: linear-gradient(to right, white, black, white)
        gradient: Some("90deg, white, black, white"),
    },
    LightIndicatorSpec {
        title: "Orange",
        description: "Mission paused",
        color: Some("#fd7e14"),
        gradient: None,
    },
    LightIndicatorSpec {
        title: "Wavering orange",
        description: "Startup signal before PC is active",
        color: None,
        // CSS: linear-gradient(to right, #fd7e14, white, #fd7e14)
        gradient: Some("90deg, #fd7e14, white, #fd7e14"),
    },
    LightIndicatorSpec {
        title: "Fading orange",
        description: "Shutting down robot",
        color: None,
        // CSS: linear-gradient(to bottom, #fd7e14, #fff3e0)
        gradient: Some("180deg, #fd7e14, #fff3e0"),
    },
    LightIndicatorSpec {
        title: "Blinking orange",
        description: "Relative move, ignoring obstacles",
        color: None,
        // CSS uses repeating-linear-gradient; approximate one pulse period.
        gradient: Some("90deg, transparent, #fd7e14 15%, #fd7e14 35%, transparent 50%"),
    },
    LightIndicatorSpec {
        title: "Wavering purple and orange",
        description: "General error",
        color: None,
        // CSS: linear-gradient(to right, #fd7e14, #a435f0, #fd7e14)
        gradient: Some("90deg, #fd7e14, #a435f0, #fd7e14"),
    },
    LightIndicatorSpec {
        title: "Blue",
        description: "Manual drive",
        color: Some("#0000ff"),
        gradient: None,
    },
    LightIndicatorSpec {
        title: "Wavering blue",
        description: "Mapping",
        color: None,
        // CSS: linear-gradient(to right, #0000ff, white, #0000ff)
        gradient: Some("90deg, #0000ff, white, #0000ff"),
    },
    LightIndicatorSpec {
        title: "Battery percentage",
        description: "Charging at charging station",
        color: None,
        // CSS: linear-gradient(to right, #ff0000, transparent 25%)
        gradient: Some("90deg, #ff0000, transparent 25%"),
    },
    LightIndicatorSpec {
        title: "Wavering cyan",
        description: "Waiting for MiR Fleet resource or for another MiR robot to move",
        color: None,
        // CSS: linear-gradient(to right, #00fafa, white, #00fafa)
        gradient: Some("90deg, #00fafa, white, #00fafa"),
    },
];

/// Description text colour — matches `.light-indicator-description` in
/// `public/globals.css` (`#6c757d`).
const LIGHT_INDICATOR_DESC_COLOR: &str = "#6c757d";

/// Bar height / radius — matches `.light-indicator-bar` (5px, 2px radius).
const LIGHT_INDICATOR_BAR_HEIGHT: f64 = 5.0;
const LIGHT_INDICATOR_BAR_RADIUS: f64 = 2.0;

/// Grid metrics — matches `.light-indicator-grid`
/// (`minmax(150px, 1fr)`, `gap: 1.25rem` ≈ 15pt).
const LIGHT_INDICATOR_MIN: f64 = 150.0;
const LIGHT_INDICATOR_GAP: f64 = 15.0;

/// Build a trivial Markdoc render node.
fn light_rt_tag(name: &str, attrs: std::collections::HashMap<String, Scalar>, children: Vec<RenderableTreeNode>) -> RenderableTreeNode {
    RenderableTreeNode::Tag(Box::new(Tag {
        name: name.to_string(),
        attributes: attrs,
        children,
    }))
}

fn light_rt_text(s: &str) -> RenderableTreeNode {
    RenderableTreeNode::Scalar(Scalar::String(s.to_string()))
}

/// One grid cell: `{% swatch %}` bar + bold title + grey description.
/// Title and description share one paragraph (hard-break between) so the
/// gap matches the web's tight `.light-indicator-title` / description stack
/// rather than two full `paragraph_space_after` gaps.
fn light_indicator_cell(spec: &LightIndicatorSpec) -> Vec<RenderableTreeNode> {
    let mut swatch_attrs = std::collections::HashMap::new();
    if let Some(g) = spec.gradient {
        swatch_attrs.insert("gradient".to_string(), Scalar::String(g.to_string()));
    } else if let Some(c) = spec.color {
        swatch_attrs.insert("color".to_string(), Scalar::String(c.to_string()));
    }
    swatch_attrs.insert(
        "height".to_string(),
        Scalar::Number(LIGHT_INDICATOR_BAR_HEIGHT),
    );
    swatch_attrs.insert(
        "radius".to_string(),
        Scalar::Number(LIGHT_INDICATOR_BAR_RADIUS),
    );

    let mut color_attrs = std::collections::HashMap::new();
    color_attrs.insert(
        "value".to_string(),
        Scalar::String(LIGHT_INDICATOR_DESC_COLOR.to_string()),
    );

    let label = light_rt_tag(
        "p",
        std::collections::HashMap::new(),
        vec![
            light_rt_tag(
                "strong",
                std::collections::HashMap::new(),
                vec![light_rt_text(spec.title)],
            ),
            light_rt_tag("hardbreak", std::collections::HashMap::new(), Vec::new()),
            light_rt_tag(
                "color",
                color_attrs,
                vec![light_rt_text(spec.description)],
            ),
        ],
    );

    vec![light_rt_tag("swatch", swatch_attrs, Vec::new()), label]
}

/// `{% lightindicators /%}` — status-light legend. Expands into a `{% grid %}`
/// of `{% swatch %}` bars with bold titles and muted descriptions, using the
/// same colours / gradients as the web `LightIndicators` component.
fn layout_lightindicators(x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let items: Vec<RenderableTreeNode> = LIGHT_INDICATORS
        .iter()
        .map(|spec| {
            light_rt_tag(
                "li",
                std::collections::HashMap::new(),
                light_indicator_cell(spec),
            )
        })
        .collect();

    let ul = light_rt_tag("ul", std::collections::HashMap::new(), items);

    let mut grid_attrs = std::collections::HashMap::new();
    grid_attrs.insert("min".to_string(), Scalar::Number(LIGHT_INDICATOR_MIN));
    grid_attrs.insert("gap".to_string(), Scalar::Number(LIGHT_INDICATOR_GAP));

    let grid = Tag {
        name: "grid".to_string(),
        attributes: grid_attrs,
        children: vec![ul],
    };
    let mut blocks = layout_grid(&grid, x, width, ctx);
    // Match `.light-indicator-grid { margin: 1.25rem 0 }` — keep a bit of
    // breathing room after the legend before the next section prose.
    if let Some(last) = blocks.last_mut() {
        last.space_after = LIGHT_INDICATOR_GAP as f32;
    }
    blocks
}

/// Outer margin of `{% noParaSpaceBox %}` — matches `.noParaSpaceBox {
/// margin: 15px 0 }` in `public/globals.css` (15 CSS px ≈ 11 pt).
const NO_PARA_SPACE_BOX_MARGIN: f32 = 11.0;

/// `{% noParaSpaceBox %}…{% /noParaSpaceBox %}` — lay children out normally,
/// then collapse every inter-paragraph `space_after` to zero so address /
/// contact lines stack as tightly as the web `.noParaSpaceBox p { margin: 0 }`
/// rule. The box itself keeps a small trailing margin (CSS `15px`).
fn layout_no_para_space_box(
    tag: &Tag,
    x: f32,
    width: f32,
    ctx: &mut LayoutCtx<'_>,
) -> Vec<Block> {
    // Skip blank / whitespace-only children so a leading blank line after the
    // opening tag doesn't leave an empty gap at the top of the stack.
    let children: Vec<&RenderableTreeNode> = tag
        .children
        .iter()
        .filter(|c| match c {
            RenderableTreeNode::Tag(t) => !node_text_is_blank(&t.children),
            RenderableTreeNode::Scalar(Scalar::String(s)) => !s.trim().is_empty(),
            RenderableTreeNode::Scalar(_) => true,
        })
        .collect();

    let mut blocks = Vec::new();
    for child in children {
        blocks.extend(layout_node(child, x, width, ctx));
    }
    if blocks.is_empty() {
        return blocks;
    }
    let last = blocks.len() - 1;
    for (i, b) in blocks.iter_mut().enumerate() {
        b.space_after = if i == last {
            NO_PARA_SPACE_BOX_MARGIN
        } else {
            0.0
        };
    }
    blocks
}

/// Light grey panel behind `{% imagegrid %}` — matches `.imagegrid {
/// background-color: #f9f9f9 }` in `public/globals.css`.
const IMAGE_GRID_BG: &str = "#f9f9f9";

/// Column gutter inside an image grid — matches `.imagegrid { gap: 10px }`.
const IMAGE_GRID_GAP: f32 = 10.0;

/// String attribute helper for gridimage / imagegrid layout.
fn attr_string<'a>(
    attrs: &'a std::collections::HashMap<String, Scalar>,
    key: &str,
) -> Option<&'a str> {
    match attrs.get(key) {
        Some(Scalar::String(s)) if !s.trim().is_empty() => Some(s.as_str()),
        _ => None,
    }
}

/// Expand one `{% gridimage %}` into the column contents: media + bold
/// headline + body paragraph (mirrors `MarkdocGridImage` on the web).
fn gridimage_cell_nodes(tag: &Tag) -> Vec<RenderableTreeNode> {
    let mut nodes = Vec::new();

    let mut media_attrs = std::collections::HashMap::new();
    if let Some(id) = attr_string(&tag.attributes, "id") {
        media_attrs.insert("id".to_string(), Scalar::String(id.to_string()));
    }
    if let Some(alt) = attr_string(&tag.attributes, "alt") {
        media_attrs.insert("alt".to_string(), Scalar::String(alt.to_string()));
    }
    // Fill the column — the locomotion examples are the main visual.
    media_attrs.insert("size".to_string(), Scalar::String("large".to_string()));
    nodes.push(light_rt_tag("media", media_attrs, Vec::new()));

    if let Some(headline) = attr_string(&tag.attributes, "headline") {
        nodes.push(light_rt_tag(
            "p",
            std::collections::HashMap::new(),
            vec![light_rt_tag(
                "strong",
                std::collections::HashMap::new(),
                vec![light_rt_text(headline)],
            )],
        ));
    }

    if let Some(body) = attr_string(&tag.attributes, "bodytext") {
        nodes.push(light_rt_tag(
            "p",
            std::collections::HashMap::new(),
            vec![light_rt_text(body)],
        ));
    }

    nodes
}

/// `{% imagegrid %}…{% /imagegrid %}` — equal columns of `{% gridimage %}`
/// cells on a light grey panel (Locomotion example, etc.).
fn layout_imagegrid(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let cells: Vec<Vec<RenderableTreeNode>> = tag
        .children
        .iter()
        .filter_map(|c| match c {
            RenderableTreeNode::Tag(t) if t.name == "gridimage" => Some(gridimage_cell_nodes(t)),
            _ => None,
        })
        .collect();

    if cells.is_empty() {
        // No gridimage children — fall back to laying out whatever is inside
        // so content is not silently dropped.
        return layout_children(&tag.children, x, width, ctx);
    }

    let bg = super::inline::parse_css_color(IMAGE_GRID_BG).unwrap_or_else(|| {
        rgb::Color::new(0xF9, 0xF9, 0xF9)
    });
    let inner_x = x + PANEL_PAD;
    let inner_w = width - 2.0 * PANEL_PAD;
    let row = layout_cells_row(
        cells,
        inner_x,
        inner_w,
        IMAGE_GRID_GAP,
        None,
        None,
        ctx,
    );
    vec![wrap_in_panel(
        row,
        x,
        width,
        bg,
        ctx.style.paragraph_space_after,
    )]
}

/// Standalone `{% gridimage %}` (outside an imagegrid) — stack media +
/// headline + body at full column width, no grey panel.
fn layout_gridimage(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let nodes = gridimage_cell_nodes(tag);
    layout_children(&nodes, x, width, ctx)
}

/// Build an SVG for a QR `code`: a `light` field with every dark module a
/// unit cell in `dark`, plus a `quiet`-module margin. One `<path>` collects
/// all dark cells, so it stays compact and scales as crisp vector.
fn qr_svg(code: &qrcodegen::QrCode, quiet: i32, dark: &str, light: &str) -> String {
    use std::fmt::Write as _;
    let n = code.size();
    let dim = n + 2 * quiet;
    let mut path = String::new();
    for y in 0..n {
        for x in 0..n {
            if code.get_module(x, y) {
                let _ = write!(path, "M{} {}h1v1h-1z", x + quiet, y + quiet);
            }
        }
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{dim}\" height=\"{dim}\" viewBox=\"0 0 {dim} {dim}\">\
         <rect width=\"{dim}\" height=\"{dim}\" fill=\"{light}\"/>\
         <path d=\"{path}\" fill=\"{dark}\"/></svg>"
    )
}

/// Encode `value` at error-correction level `ecl` (low | medium | quartile |
/// high) and return the SVG for the code, or `None` if the value is too long
/// to fit any QR version. Shared by the `{% qr %}` tag and the style-driven
/// last-page stamp.
pub(super) fn qr_svg_string(
    value: &str,
    ecl: &str,
    quiet: i32,
    dark: &str,
    light: &str,
) -> Option<String> {
    let ecl = match ecl.trim().to_ascii_lowercase().as_str() {
        "low" | "l" => qrcodegen::QrCodeEcc::Low,
        "quartile" | "q" => qrcodegen::QrCodeEcc::Quartile,
        "high" | "h" => qrcodegen::QrCodeEcc::High,
        _ => qrcodegen::QrCodeEcc::Medium,
    };
    let code = qrcodegen::QrCode::encode_text(value, ecl).ok()?;
    Some(qr_svg(&code, quiet.max(0), dark, light))
}

/// `{% qr %}` — a QR code from `value`. `size` (points, default 72) sets the
/// square's side (capped at the available width); `ecl` (low | medium |
/// quartile | high, default medium) the error-correction level; `color` /
/// `background` the module / field colours; `quiet_zone` (modules, default 4)
/// the margin; `align` (left | center | right) the horizontal placement.
/// Rendered as a synthesised SVG, so it stays crisp vector at any scale —
/// e.g. a small document-number code on a printed guide's last page.
fn layout_qr(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    // `value` is usually a string, but a frontmatter integer such as
    // `documentNumber` arrives as a Number (YAML ints are f64) — encode it
    // without a spurious decimal.
    let value = match tag.attributes.get("value") {
        Some(Scalar::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(Scalar::Number(n)) if n.is_finite() => {
            if n.fract() == 0.0 {
                format!("{}", *n as i64)
            } else {
                n.to_string()
            }
        }
        _ => return placeholder(x, width, ctx, "[qr: missing value]"),
    };
    let ecl = match tag.attributes.get("ecl") {
        Some(Scalar::String(s)) => s.clone(),
        _ => "medium".to_string(),
    };
    let quiet = (attr_points(&tag.attributes, "quiet_zone", 4.0).round() as i32).max(0);
    let hex = |key: &str, fallback: &str| {
        tag.attributes
            .get(key)
            .and_then(|v| match v {
                Scalar::String(s) => css_hex(s),
                _ => None,
            })
            .unwrap_or_else(|| fallback.to_string())
    };
    let svg = match qr_svg_string(
        &value,
        &ecl,
        quiet,
        &hex("color", "#000000"),
        &hex("background", "#ffffff"),
    ) {
        Some(s) => s,
        None => return placeholder(x, width, ctx, "[qr: value too long to encode]"),
    };
    let size = attr_points(&tag.attributes, "size", 72.0).clamp(8.0, width.max(8.0));
    let align = parse_layout_align(&tag.attributes).or(ctx.cell_content_align);
    let draw_x = align_within(x, width, size, align);
    match usvg::Tree::from_data(svg.as_bytes(), &usvg::Options::default()) {
        Ok(tree) => vec![Block {
            height: size,
            space_after: ctx.style.paragraph_space_after,
            draw: BlockDraw::Svg {
                tree: Arc::new(tree),
                x: draw_x,
                width: size,
                height: size,
                caption: None,
            },
            outline: None,
            anchor_id: None,
            tag_role: None,
            page_column: 0,
            column_span: false,
        }],
        Err(e) => {
            eprintln!("warning: qr render failed — {e}");
            placeholder(x, width, ctx, "[qr: render failed]")
        }
    }
}

/// Return the inner `img`/`media` tag of `node`, whether it is that tag
/// itself or a `<p>`/`<div>` wrapping one (pulldown-cmark wraps a bare
/// image in a paragraph). Used by `{% float %}` to find the figure.
fn find_media_tag(node: &RenderableTreeNode) -> Option<&Tag> {
    if let RenderableTreeNode::Tag(t) = node {
        if t.name == "img" || t.name == "media" {
            return Some(t.as_ref());
        }
        if t.name == "p" || t.name == "div" {
            return t.children.iter().find_map(find_media_tag);
        }
    }
    None
}

/// Resolve a `{% float width=… %}` value to an image width in points,
/// given the column width `col`. A number `≤ 1` (or a `"NN%"` string) is a
/// fraction of the column; a number `> 1` is an explicit point value. The
/// result is clamped so the image always leaves room for text. Default 40%.
fn resolve_float_width(attr: Option<&Scalar>, col: f32) -> f32 {
    let raw = match attr {
        Some(Scalar::Number(v)) => {
            let v = *v as f32;
            if v <= 1.0 { col * v } else { v }
        }
        Some(Scalar::String(s)) => {
            let s = s.trim();
            if let Some(pct) = s.strip_suffix('%') {
                pct.trim()
                    .parse::<f32>()
                    .ok()
                    .map(|p| col * p / 100.0)
                    .unwrap_or(col * 0.4)
            } else {
                match s.parse::<f32>() {
                    Ok(v) if v <= 1.0 => col * v,
                    Ok(v) => v,
                    Err(_) => col * 0.4,
                }
            }
        }
        _ => col * 0.4,
    };
    raw.clamp(24.0, col * 0.8)
}

/// Is `node` a paragraph of purely inline content — a `<p>` with no
/// block-level child to promote (`img` / `media` / `callout`)? Such a
/// paragraph can be reflowed line-by-line around a float; anything else is
/// laid out by the normal dispatch at the width available where it starts.
fn is_plain_paragraph(node: &RenderableTreeNode) -> bool {
    match node {
        RenderableTreeNode::Tag(t) if t.name == "p" => !t.children.iter().any(|c| {
            matches!(
                c,
                RenderableTreeNode::Tag(ct)
                    if matches!(ct.name.as_str(), "img" | "media" | "callout")
            )
        }),
        _ => false,
    }
}

/// Lay out a plain paragraph that wraps around a float. Mirrors
/// [`layout_paragraph`]'s inline collection, link styling and hyphenation,
/// but breaks lines to hug the image: lines above `exclude_h` (measured from
/// this paragraph's own top) use `narrow_w` shifted by `narrow_x`; lines
/// below span `full_w` at the column-left origin `x`. Footnotes and
/// mid-paragraph anchors inside it are preserved.
#[allow(clippy::too_many_arguments)]
fn layout_paragraph_float(
    node: &RenderableTreeNode,
    x: f32,
    full_w: f32,
    narrow_x: f32,
    narrow_w: f32,
    exclude_h: f32,
    ctx: &mut LayoutCtx<'_>,
) -> Vec<Block> {
    let RenderableTreeNode::Tag(tag) = node else {
        return Vec::new();
    };
    let mut inlines =
        Inlines::from_with_labels(&tag.children, &mut ctx.footnotes, Some(ctx.crossref_labels));
    if inlines.text.trim().is_empty() {
        return Vec::new();
    }
    // Link text styling — identical to `layout_paragraph`.
    let link_style = &ctx.style.link;
    let link_color: krilla::color::rgb::Color = link_style.color.into();
    for link in &inlines.links {
        inlines.style_ranges.push(InlineRange {
            start: link.start,
            end: link.end,
            prop: InlineProp::Color(link_color),
        });
        if link_style.italic {
            inlines.style_ranges.push(InlineRange {
                start: link.start,
                end: link.end,
                prop: InlineProp::Italic,
            });
        }
        if link_style.bold {
            inlines.style_ranges.push(InlineRange {
                start: link.start,
                end: link.end,
                prop: InlineProp::Bold,
            });
        }
    }
    if link_style.underline {
        for link in &inlines.links {
            inlines.style_ranges.push(InlineRange {
                start: link.start,
                end: link.end,
                prop: InlineProp::Underline {
                    thickness: link_style.underline_thickness,
                },
            });
        }
    }
    // Hyphenate only when nothing keys on byte offsets (same guard as
    // `layout_paragraph`) — helps fill the narrow lines beside the image.
    if let Some(h) = ctx.hyphenator
        && inlines.style_ranges.is_empty()
        && inlines.links.is_empty()
        && inlines.mid_anchors.is_empty()
        && inlines.footnote_calls.is_empty()
    {
        inlines.text = h.hyphenate(&inlines.text);
    }
    let style = TextStyle {
        font_size: ctx.style.body_font_size,
        font_weight: 400.0,
        line_height: ctx.style.body_line_height,
        color: ctx.style.text_color.into(),
        font_families: ctx.body_families,
        italic: false,
    };
    let layout = build_layout_float(
        &inlines.text,
        &inlines.style_ranges,
        &style,
        full_w,
        narrow_w,
        narrow_x,
        exclude_h,
        ctx.font_cx,
        ctx.layout_cx,
    );
    let slice = TextSlice::whole_with_extras(
        layout,
        inlines.text,
        inlines.links,
        inlines.mid_anchors,
        inlines.footnote_calls,
        x,
    );
    let height = slice.height();
    vec![Block {
        height,
        space_after: ctx.style.paragraph_space_after,
        draw: BlockDraw::Text(slice),
        outline: None,
        anchor_id: None,
        tag_role: None,
        page_column: 0,
        column_span: false,
    }]
}

/// `{% float %}` — place an image on one side (`side="left"|"right"`,
/// default left) and wrap the following content around it. `width` sizes the
/// image (a fraction `≤ 1` of the column, an explicit point value `> 1`, or
/// a `"NN%"` string; default 40 %); `gap` is the space between image and
/// content (points, default 14).
///
/// The wrap keeps its real block structure. Each block after the image is
/// laid out at the width available where it starts: while it sits beside the
/// image it is narrowed and shifted to clear the picture; once past the
/// image's bottom edge it spans the full column. A plain paragraph that
/// straddles the boundary reflows line-by-line (narrow, then full). Lists,
/// code blocks, callouts, tables and nested figures render as themselves,
/// and footnotes / anchors inside the wrap are preserved.
///
/// The whole float is one indivisible pagination unit — it never straddles a
/// page break (if it doesn't fit, it moves to the next page). The image
/// never upscales, so a small raster bounds the float width to its natural
/// size (as everywhere else). For fully independent side-by-side lanes use
/// `{% columns %}` instead.
///
/// When one or more images inside the region carry an explicit `side`
/// attribute (`{% media src=… side="left" /%}`), this switches to the
/// **anchored** mode ([`layout_float_anchored`]): several images floated to
/// either side, anchored where they appear in the prose, with one continuous
/// text stream wrapping around all of them (a magazine layout).
fn layout_float(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    // Any image with an explicit `side` → the multi-image, inline-anchored
    // magazine mode. Otherwise the single leading-image form below.
    if has_anchored_floats(&tag.children) {
        return layout_float_anchored(tag, x, width, ctx);
    }

    // The first image among the children is the float; everything after it
    // wraps. No image → nothing to float around, so lay the children out
    // normally.
    let Some(fig_idx) = tag
        .children
        .iter()
        .position(|c| find_media_tag(c).is_some())
    else {
        return layout_children(&tag.children, x, width, ctx);
    };

    let col = width;
    let side_right = matches!(
        tag.attributes.get("side"),
        Some(Scalar::String(s)) if s.trim() == "right"
    );
    let gap = tag
        .attributes
        .get("gap")
        .and_then(|s| match s {
            Scalar::Number(v) => Some(*v as f32),
            Scalar::String(s) => s.trim().parse::<f32>().ok(),
            _ => None,
        })
        .unwrap_or(14.0)
        .max(0.0);
    let img_target_w = resolve_float_width(tag.attributes.get("width"), col);

    // Lay out the image at the target width. A missing / undecodable file
    // yields a text placeholder, not a figure — fall back to normal flow.
    let media_tag = find_media_tag(&tag.children[fig_idx]).unwrap();
    let mut img_blocks = layout_media(media_tag, x, img_target_w, ctx);
    let (img_w, img_h) = match img_blocks.first().map(|b| &b.draw) {
        Some(BlockDraw::Image { width, height, .. } | BlockDraw::Svg { width, height, .. }) => {
            (*width, *height)
        }
        _ => return layout_children(&tag.children, x, width, ctx),
    };
    let mut img_block = img_blocks.remove(0);

    let wrap_children = &tag.children[fig_idx + 1..];

    // Text region beside the image. If it is too narrow to wrap sensibly,
    // or there is nothing to wrap, stack the image over the content instead.
    let narrow_w = col - img_w - gap;
    if wrap_children.is_empty() || narrow_w < ctx.style.body_font_size * 6.0 {
        let mut out = vec![img_block];
        out.extend(layout_children(wrap_children, x, col, ctx));
        return out;
    }
    let (narrow_x, img_x) = if side_right {
        (0.0, x + col - img_w)
    } else {
        (img_w + gap, x)
    };
    // Move the image onto the chosen side (left already sits at `x`).
    match &mut img_block.draw {
        BlockDraw::Image { x: ix, .. } | BlockDraw::Svg { x: ix, .. } => *ix = img_x,
        _ => {}
    }
    // A little air below the image so the first full-width line clears its
    // bottom edge.
    let exclude_h = img_h + ctx.style.body_font_size * 0.3;

    // Flow the wrap blocks. Phase 1 — blocks that start beside the image are
    // laid out narrow (and shifted); a plain paragraph reflows so its tail
    // widens once past the image. Phase 2 — once we clear the image, lay the
    // rest out full-width in one pass so caption/figure pairing and spacing
    // stay intact. The running `y` mirrors `emit_blocks`' cumulative stacking
    // so the width chosen at layout time matches where each block is drawn.
    let mut wrap: Vec<Block> = Vec::new();
    let mut y = 0.0_f32;
    let mut k = 0;
    while k < wrap_children.len() && y < exclude_h {
        let child = &wrap_children[k];
        let mut blocks = if is_plain_paragraph(child) {
            layout_paragraph_float(child, x, col, narrow_x, narrow_w, exclude_h - y, ctx)
        } else {
            layout_node(child, x + narrow_x, narrow_w, ctx)
        };
        for b in &blocks {
            y += b.height + b.space_after;
        }
        wrap.append(&mut blocks);
        k += 1;
    }
    if k < wrap_children.len() {
        wrap.extend(layout_children(&wrap_children[k..], x, col, ctx));
    }

    // Nothing rendered (e.g. only whitespace) → plain image block.
    if wrap.is_empty() {
        return vec![img_block];
    }

    // Float content height = max of the image and the wrap's drawn extent
    // (cumulative block heights + inter-block spacing, excluding the trailing
    // `space_after`, which the float's own `space_after` covers).
    let mut wrap_bottom = 0.0_f32;
    let mut acc = 0.0_f32;
    for b in &wrap {
        acc += b.height;
        wrap_bottom = acc;
        acc += b.space_after;
    }

    vec![Block {
        height: img_h.max(wrap_bottom),
        space_after: ctx.style.paragraph_space_after,
        draw: BlockDraw::Float {
            image: Box::new(img_block),
            wrap,
        },
        outline: None,
        anchor_id: None,
        tag_role: None,
        page_column: 0,
        column_span: false,
    }]
}

/// Does any image in this subtree carry an explicit `side` attribute? That
/// marks the anchored / magazine `{% float %}` mode.
fn has_anchored_floats(children: &[RenderableTreeNode]) -> bool {
    fn node_has(node: &RenderableTreeNode) -> bool {
        if let RenderableTreeNode::Tag(t) = node {
            if (t.name == "img" || t.name == "media") && t.attributes.contains_key("side") {
                return true;
            }
            return t.children.iter().any(node_has);
        }
        false
    }
    children.iter().any(node_has)
}

/// Read a `side="left"|"right"` attribute; `Some(true)` for right, `Some(false)`
/// for left, `None` if absent or unrecognised (callers default to left).
fn media_side_right(tag: &Tag) -> Option<bool> {
    match tag.attributes.get("side") {
        Some(Scalar::String(s)) => match s.trim() {
            "right" => Some(true),
            "left" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Append `src`'s inline content onto `dst`, shifting every byte-keyed span
/// (ranges, links, anchors, footnote calls) past `dst`'s current length.
fn append_inlines(dst: &mut Inlines, src: Inlines) {
    let base = dst.text.len();
    dst.text.push_str(&src.text);
    for mut r in src.style_ranges {
        r.start += base;
        r.end += base;
        dst.style_ranges.push(r);
    }
    for mut l in src.links {
        l.start += base;
        l.end += base;
        dst.links.push(l);
    }
    for mut m in src.mid_anchors {
        m.byte_offset += base;
        dst.mid_anchors.push(m);
    }
    for mut f in src.footnote_calls {
        f.byte_offset += base;
        dst.footnote_calls.push(f);
    }
}

/// Flatten a `{% float %}` region's children into one prose stream, recording
/// each image as a float anchor `(byte_offset, tag)` at the point it appears.
/// Paragraphs are separated by a blank line; images (inline within a sentence
/// or block-level between paragraphs) contribute no text, only an anchor.
fn flatten_anchored(
    children: &[RenderableTreeNode],
    ctx: &mut LayoutCtx<'_>,
) -> (Inlines, Vec<(usize, Tag)>) {
    fn run(
        children: &[RenderableTreeNode],
        ib: &mut Inlines,
        anchors: &mut Vec<(usize, Tag)>,
        ctx: &mut LayoutCtx<'_>,
    ) {
        for c in children {
            match c {
                RenderableTreeNode::Tag(t) if t.name == "img" || t.name == "media" => {
                    anchors.push((ib.text.len(), (**t).clone()));
                }
                other => {
                    let sub = Inlines::from_with_labels(
                        std::slice::from_ref(other),
                        &mut ctx.footnotes,
                        Some(ctx.crossref_labels),
                    );
                    append_inlines(ib, sub);
                }
            }
        }
    }

    let mut ib = Inlines::new();
    let mut anchors: Vec<(usize, Tag)> = Vec::new();
    for child in children {
        match child {
            RenderableTreeNode::Tag(t) if t.name == "p" || t.name == "div" => {
                if !ib.text.is_empty() {
                    ib.text.push_str("\n\n");
                }
                run(&t.children, &mut ib, &mut anchors, ctx);
            }
            RenderableTreeNode::Tag(t) if t.name == "img" || t.name == "media" => {
                // Bare block-level media between paragraphs.
                anchors.push((ib.text.len(), (**t).clone()));
            }
            other => {
                if !ib.text.is_empty() {
                    ib.text.push_str("\n\n");
                }
                let sub = Inlines::from_with_labels(
                    std::slice::from_ref(other),
                    &mut ctx.footnotes,
                    Some(ctx.crossref_labels),
                );
                append_inlines(&mut ib, sub);
            }
        }
    }
    (ib, anchors)
}

/// Anchored / magazine `{% float %}` — several images floated to either side
/// (`{% media src=… side="left|right" width=… /%}`), each anchored where it
/// appears in the prose, with one continuous text stream wrapping around all
/// of them. See [`build_layout_float_anchored`] for the per-line mechanism.
///
/// Prose-only: rich blocks between the floats are flattened to paragraphs (for
/// structured content beside a single image use the leading-image `{% float %}`
/// form). Footnotes and mid-paragraph anchors in the prose are preserved.
fn layout_float_anchored(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let col = width;
    let gap = tag
        .attributes
        .get("gap")
        .and_then(|s| match s {
            Scalar::Number(v) => Some(*v as f32),
            Scalar::String(s) => s.trim().parse::<f32>().ok(),
            _ => None,
        })
        .unwrap_or(14.0)
        .max(0.0);

    let (mut inlines, anchor_tags) = flatten_anchored(&tag.children, ctx);
    if anchor_tags.is_empty() || inlines.text.trim().is_empty() {
        return layout_children(&tag.children, x, width, ctx);
    }

    // Lay out each anchored image; keep specs and image blocks aligned.
    let mut specs: Vec<FloatSpec> = Vec::new();
    let mut images: Vec<Block> = Vec::new();
    for (offset, mtag) in &anchor_tags {
        let side_right = media_side_right(mtag).unwrap_or(false);
        let target_w = resolve_float_width(mtag.attributes.get("width"), col);
        let mut blocks = layout_media(mtag, x, target_w, ctx);
        let (w, h) = match blocks.first().map(|b| &b.draw) {
            Some(BlockDraw::Image { width, height, .. } | BlockDraw::Svg { width, height, .. }) => {
                (*width, *height)
            }
            // Missing / undecodable image → drop this anchor (its placeholder
            // block is discarded; the prose still flows).
            _ => continue,
        };
        specs.push(FloatSpec {
            offset: *offset,
            side_right,
            width: w,
            height: h,
        });
        images.push(blocks.remove(0));
    }

    // Nothing floatable, or the widest float leaves too little measure → fall
    // back to normal stacked flow rather than an unreadable ribbon of text.
    let max_reserve = specs.iter().map(|s| s.width + gap).fold(0.0_f32, f32::max);
    if specs.is_empty() || col - max_reserve < ctx.style.body_font_size * 6.0 {
        return layout_children(&tag.children, x, width, ctx);
    }

    // Link text styling (colour / weight / underline), as paragraphs do.
    let link_style = &ctx.style.link;
    let link_color: krilla::color::rgb::Color = link_style.color.into();
    for link in &inlines.links {
        inlines.style_ranges.push(InlineRange {
            start: link.start,
            end: link.end,
            prop: InlineProp::Color(link_color),
        });
        if link_style.italic {
            inlines.style_ranges.push(InlineRange {
                start: link.start,
                end: link.end,
                prop: InlineProp::Italic,
            });
        }
        if link_style.bold {
            inlines.style_ranges.push(InlineRange {
                start: link.start,
                end: link.end,
                prop: InlineProp::Bold,
            });
        }
    }
    if link_style.underline {
        for link in &inlines.links {
            inlines.style_ranges.push(InlineRange {
                start: link.start,
                end: link.end,
                prop: InlineProp::Underline {
                    thickness: link_style.underline_thickness,
                },
            });
        }
    }
    // No hyphenation here: it would insert soft hyphens and shift the float
    // anchor byte offsets recorded above.

    let style = TextStyle {
        font_size: ctx.style.body_font_size,
        font_weight: 400.0,
        line_height: ctx.style.body_line_height,
        color: ctx.style.text_color.into(),
        font_families: ctx.body_families,
        italic: false,
    };
    let (layout, placements) = build_layout_float_anchored(
        &inlines.text,
        &inlines.style_ranges,
        &style,
        col,
        gap,
        &specs,
        ctx.font_cx,
        ctx.layout_cx,
    );
    let text_slice = TextSlice::whole_with_extras(
        layout,
        inlines.text,
        inlines.links,
        inlines.mid_anchors,
        inlines.footnote_calls,
        x,
    );
    let mut region_bottom = text_slice.height();

    // Position each image (absolute x, region-relative y) and grow the region
    // to cover any float that hangs below the text.
    let mut floats: Vec<FloatItem> = Vec::new();
    for (i, mut img) in images.into_iter().enumerate() {
        let p = placements[i];
        match &mut img.draw {
            BlockDraw::Image { x: ix, .. } | BlockDraw::Svg { x: ix, .. } => *ix = x + p.x,
            _ => {}
        }
        region_bottom = region_bottom.max(p.y + specs[i].height);
        floats.push(FloatItem {
            image: Box::new(img),
            y_offset: p.y,
        });
    }

    vec![Block {
        height: region_bottom,
        space_after: ctx.style.paragraph_space_after,
        draw: BlockDraw::FloatRegion {
            text: text_slice,
            floats,
        },
        outline: None,
        anchor_id: None,
        tag_role: None,
        page_column: 0,
        column_span: false,
    }]
}

/// `{% input %}` — a print form field: a label above a ruled box (pure
/// graphics, so PDF/A-safe — no interactive widget and no JavaScript, which
/// PDF/A forbids). The box width follows `maxlength` (roughly one character
/// per unit, capped to the column); without it the box fills the measure.
/// `type="checkbox"` draws a small square. A small grey hint below the box
/// spells out the type / length / range constraints for whoever fills the
/// form by hand, since a static PDF cannot enforce them.
fn layout_input(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let attr_str = |k: &str| match tag.attributes.get(k) {
        Some(Scalar::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    };
    let attr_num = |k: &str| match tag.attributes.get(k) {
        Some(Scalar::Number(n)) => Some(*n),
        Some(Scalar::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    // Display form of a value for the hint (keeps date-like strings verbatim).
    let attr_disp = |k: &str| match tag.attributes.get(k) {
        Some(Scalar::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(Scalar::Number(n)) => Some(if n.fract() == 0.0 {
            format!("{}", *n as i64)
        } else {
            format!("{n}")
        }),
        _ => None,
    };
    let required = matches!(tag.attributes.get("required"), Some(Scalar::Boolean(true)))
        || matches!(tag.attributes.get("required"),
            Some(Scalar::String(s)) if s.trim().eq_ignore_ascii_case("true"));

    let ty = attr_str("type").unwrap_or_else(|| "text".to_string());
    let is_checkbox = ty == "checkbox";
    let label_txt = attr_str("label")
        .or_else(|| attr_str("name"))
        .unwrap_or_default();

    let fs = ctx.style.body_font_size;
    let border: krilla::color::rgb::Color = ctx.style.blockquote_text_color.into();
    let thickness = 0.75;

    // Label, with a red required marker appended.
    let label_style = TextStyle {
        font_size: fs,
        font_weight: 400.0,
        line_height: ctx.style.body_line_height,
        color: ctx.style.text_color.into(),
        font_families: ctx.body_families,
        italic: false,
    };
    let label_slice = if label_txt.is_empty() {
        None
    } else {
        let mut text = label_txt.clone();
        let mut ranges: Vec<InlineRange> = Vec::new();
        if required {
            let star_start = text.len() + 1; // after the joining space
            text.push_str(" *");
            ranges.push(InlineRange {
                start: star_start,
                end: text.len(),
                prop: InlineProp::Color(krilla::color::rgb::Color::new(200, 45, 45)),
            });
        }
        let layout = build_layout(
            &text,
            &ranges,
            &label_style,
            width,
            ctx.font_cx,
            ctx.layout_cx,
        );
        Some(TextSlice::whole(layout, text, Vec::new(), x))
    };
    let label_h = label_slice.as_ref().map(|s| s.height()).unwrap_or(0.0);

    // Field box: below the label. Sized by `maxlength`, else fills the width.
    let gap1 = fs * 0.3;
    let field_y = label_h + gap1;
    let (field_w, field_h) = if is_checkbox {
        let s = fs * 1.05;
        (s, s)
    } else {
        let char_w = fs * 0.62;
        let desired = match attr_num("maxlength") {
            Some(ml) if ml > 0.0 => (ml as f32 * char_w).max(char_w * 3.0),
            _ => width,
        };
        let fw = desired.min(width).max((fs * 3.0).min(width));
        (fw, fs * 1.5)
    };

    // Constraint hint: what the person filling it in needs to know.
    let mut parts: Vec<String> = Vec::new();
    if ty != "text" && ty != "checkbox" {
        parts.push(ty.clone());
    }
    match (attr_num("minlength"), attr_num("maxlength")) {
        (Some(a), Some(b)) => parts.push(format!("{}\u{2013}{} chars", a as i64, b as i64)),
        (None, Some(b)) => parts.push(format!("max {} chars", b as i64)),
        (Some(a), None) => parts.push(format!("min {} chars", a as i64)),
        (None, None) => {}
    }
    match (attr_disp("min"), attr_disp("max")) {
        (Some(a), Some(b)) => parts.push(format!("{a}\u{2013}{b}")),
        (None, Some(b)) => parts.push(format!("\u{2264} {b}")),
        (Some(a), None) => parts.push(format!("\u{2265} {a}")),
        (None, None) => {}
    }
    let hint_str = parts.join("  \u{b7}  ");
    let hint_slice = if hint_str.is_empty() {
        None
    } else {
        let hint_style = TextStyle {
            font_size: fs * 0.82,
            font_weight: 400.0,
            line_height: ctx.style.body_line_height,
            color: ctx.style.blockquote_text_color.into(),
            font_families: ctx.body_families,
            italic: true,
        };
        let layout = build_layout(
            &hint_str,
            &[],
            &hint_style,
            width,
            ctx.font_cx,
            ctx.layout_cx,
        );
        Some(TextSlice::whole(layout, hint_str, Vec::new(), x))
    };

    let gap2 = fs * 0.25;
    let hint_y = field_y + field_h + gap2;
    let height = match &hint_slice {
        Some(h) => hint_y + h.height(),
        None => field_y + field_h,
    };

    vec![Block {
        height,
        space_after: ctx.style.paragraph_space_after,
        draw: BlockDraw::FormField {
            label: label_slice,
            field_x: x,
            field_y,
            field_w,
            field_h,
            border,
            thickness,
            hint: hint_slice,
            hint_y,
        },
        outline: None,
        anchor_id: None,
        tag_role: None,
        page_column: 0,
        column_span: false,
    }]
}

fn layout_table(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    let column_span = ctx.page_columns() > 1;
    let lay_w = if column_span { ctx.body_full_width() } else { width };
    // Per-table style overrides from the `{% table … %}` attributes; a
    // plain pipe table has none, so everything inherits the document style.
    let ov = TableOverride::from_attrs(&tag.attributes);
    // For a plain pipe table the rows are this node's own children; when a
    // pipe table is wrapped in `{% table … %}` for styling, the real
    // <table> is a child — descend to wherever the rows actually live.
    let rows_tag = table_rows_source(tag);

    // Collect rows from <thead> and <tbody>. pulldown-cmark's pipe-table
    // form puts cells directly under <thead> (no wrapping <tr>) for the
    // header, and emits <tr>s as direct children of <table> for body
    // rows. We tolerate both shapes:
    //   <table><thead><tr><td/></tr></thead><tbody><tr>…</tr></tbody></table>
    //   <table><thead><td/><td/></thead><tr/><tr/></table>
    let mut header_rows: Vec<RowSource<'_>> = Vec::new();
    let mut body_rows: Vec<RowSource<'_>> = Vec::new();
    for child in &rows_tag.children {
        let RenderableTreeNode::Tag(t) = child else {
            continue;
        };
        match t.name.as_str() {
            "thead" => {
                // If <thead> has any child cell directly, treat thead as
                // one row; otherwise look for <tr> children.
                if t.children.iter().any(is_cell_node) {
                    header_rows.push(RowSource::CellsDirect(t));
                } else {
                    for tr in t.children.iter().filter_map(as_tr) {
                        header_rows.push(RowSource::Row(tr));
                    }
                }
            }
            "tbody" => {
                for tr in t.children.iter().filter_map(as_tr) {
                    body_rows.push(RowSource::Row(tr));
                }
            }
            "tr" => body_rows.push(RowSource::Row(t)),
            _ => {}
        }
    }
    if header_rows.is_empty() && body_rows.is_empty() {
        return Vec::new();
    }
    // A markdown table must declare a header row; when that row is wholly
    // empty (the header-less metadata table authored as `| | |`), drop it
    // so the table renders without a stray empty band and rule on top.
    if !header_rows.is_empty() && rows_are_blank(&header_rows) {
        header_rows.clear();
    }
    // Drop wholly-blank body rows. A pipe table wrapped in `{% table %}`
    // can pick up a spurious empty trailing row from the closing tag; a row
    // with no content in any cell is never meaningful. (A row with some
    // empty cells but text elsewhere — e.g. a metadata row with a blank
    // value — is kept.)
    body_rows.retain(|r| !r.cells().all(|c| node_text_is_blank(&c.children)));

    // Column count under the occupancy model (a `colspan` widens a row; a
    // `rowspan` reserves columns for following rows).
    let num_cols = grid_num_cols(&header_rows, &body_rows);
    if num_cols == 0 {
        return Vec::new();
    }

    let padding = ov.cell_padding.unwrap_or(ctx.style.table_cell_padding);
    let border_thickness = ctx.style.table_border_thickness;
    let total_borders = border_thickness * (num_cols as f32 + 1.0);
    let inner_width = lay_w - total_borders;

    // Decide column widths. Explicit weights win when they match this
    // table's column count; otherwise fall back to the automatic modes.
    let weights = ov
        .column_weights
        .as_deref()
        .unwrap_or(ctx.style.table_column_weights.as_slice());
    let column_widths = match weighted_column_widths(weights, inner_width) {
        Some(ws) if ws.len() == num_cols => ws,
        _ => match ctx.style.table_column_sizing {
            TableColumnSizing::Equal => vec![inner_width / num_cols as f32; num_cols],
            TableColumnSizing::Auto => compute_auto_column_widths(
                &header_rows,
                &body_rows,
                num_cols,
                inner_width,
                padding,
                ctx,
            ),
        },
    };

    // Compute per-column x origins (left edge of each column's content area).
    let mut column_xs = Vec::with_capacity(num_cols);
    let mut cursor = x + border_thickness;
    for w in &column_widths {
        column_xs.push(cursor);
        cursor += w + border_thickness;
    }

    // Render the first column as row headers (bold + header fill)?
    let header_column = ov.header_column.unwrap_or(ctx.style.table_header_column);

    // Per-column text alignment from CommonMark's `:--`/`--:` delimiter row,
    // which markdoc stores on the table node's `align` attribute.
    let column_aligns: Vec<parley::layout::Alignment> = match rows_tag.attributes.get("align") {
        Some(Scalar::Array(arr)) => arr
            .iter()
            .map(|s| match s {
                Scalar::String(a) if a == "center" => parley::layout::Alignment::Center,
                Scalar::String(a) if a == "right" => parley::layout::Alignment::End,
                _ => parley::layout::Alignment::Start,
            })
            .collect(),
        _ => Vec::new(),
    };

    // Rowspan occupancy carries across rows (header → body); each row's
    // layout consumes and ages it.
    let mut occupied = vec![0usize; num_cols];
    let mut rows_out: Vec<TableRow> = Vec::new();
    for r in header_rows.iter() {
        rows_out.push(layout_row_source(
            r,
            true,
            &column_xs,
            &column_widths,
            padding,
            header_column,
            &column_aligns,
            &mut occupied,
            ctx,
        ));
    }
    for r in body_rows.iter() {
        rows_out.push(layout_row_source(
            r,
            false,
            &column_xs,
            &column_widths,
            padding,
            header_column,
            &column_aligns,
            &mut occupied,
            ctx,
        ));
    }

    // Zebra striping: paint every other body row with the effective stripe
    // colour — a per-table `stripe="…"` override, else the document
    // default. `stripe="none"` switches it off for this one table.
    let stripe = match ov.stripe {
        Some(s) => s,
        None => ctx.style.table_stripe_color.map(Into::into),
    };
    if let Some(stripe) = stripe {
        for (body_idx, row) in rows_out.iter_mut().filter(|r| !r.is_header).enumerate() {
            if body_idx % 2 == 1 {
                row.fill = Some(stripe);
            }
        }
    }

    let table_height: f32 = rows_out.iter().map(|r| r.height).sum::<f32>()
        + border_thickness * (rows_out.len() as f32 + 1.0);

    let table_id = ctx.next_table_id();

    // Effective table-level visuals: per-table overrides fall back to the
    // document style.
    let header_bg = ov
        .header_bg
        .unwrap_or_else(|| ctx.style.table_header_background.into());
    let border_color = ov
        .border_color
        .unwrap_or_else(|| ctx.style.table_border_color.into());
    let border_style = ov.borders.unwrap_or(ctx.style.table_borders);
    let edge = match ov.edge_color {
        Some(c) => Some((
            c,
            ctx.style.table_edge_thickness.unwrap_or(border_thickness),
        )),
        None => ctx.style.table_edge_color.map(|c| {
            (
                c.into(),
                ctx.style.table_edge_thickness.unwrap_or(border_thickness),
            )
        }),
    };

    vec![Block {
        height: table_height,
        space_after: ctx.style.table_space_after,
        draw: BlockDraw::Table {
            x,
            column_widths,
            rows: rows_out,
            cell_padding: padding,
            header_bg,
            border_color,
            border_thickness,
            border_style,
            edge,
            caption: ctx.pending_table_caption.take(),
        },
        outline: None,
        anchor_id: Some(format!("__table_{table_id}")),
        tag_role: None,
        page_column: 0,
        column_span,
    }]
}

/// A row source — either a real <tr> wrapper, or a section (e.g. <thead>)
/// whose direct children are the cells (pulldown-cmark's pipe-table form).
enum RowSource<'a> {
    Row(&'a Tag),
    CellsDirect(&'a Tag),
}

impl<'a> RowSource<'a> {
    fn cells(&self) -> impl Iterator<Item = &'a Tag> + 'a {
        let parent: &'a Tag = match self {
            RowSource::Row(t) => t,
            RowSource::CellsDirect(t) => t,
        };
        parent.children.iter().filter_map(|c| {
            if let RenderableTreeNode::Tag(t) = c
                && (t.name == "th" || t.name == "td")
            {
                return Some(t.as_ref());
            }
            None
        })
    }
}

fn is_cell_node(c: &RenderableTreeNode) -> bool {
    matches!(c, RenderableTreeNode::Tag(t) if t.name == "th" || t.name == "td")
}

fn as_tr(c: &RenderableTreeNode) -> Option<&Tag> {
    if let RenderableTreeNode::Tag(t) = c
        && t.name == "tr"
    {
        return Some(t.as_ref());
    }
    None
}

/// True when every cell across `rows` is visually empty — the case
/// markdown's mandatory header row produces for a header-less metadata
/// table (`| | |`). Used to drop such a header so it leaves no stray
/// band or rule.
fn rows_are_blank(rows: &[RowSource<'_>]) -> bool {
    rows.iter()
        .all(|r| r.cells().all(|c| node_text_is_blank(&c.children)))
}

/// Recursively true when a node subtree carries no visible content — no
/// non-whitespace text and no image/media tag.
fn node_text_is_blank(nodes: &[RenderableTreeNode]) -> bool {
    nodes.iter().all(|n| match n {
        RenderableTreeNode::Scalar(Scalar::String(s)) => s.trim().is_empty(),
        // Numbers / bools render as visible text.
        RenderableTreeNode::Scalar(_) => false,
        RenderableTreeNode::Tag(t) => {
            !matches!(t.name.as_str(), "img" | "media") && node_text_is_blank(&t.children)
        }
    })
}

/// A cell's column span from its `colspan` attribute (set by the list-syntax
/// `{% colspan=N %}` annotation); at least 1.
fn cell_colspan(tag: &Tag) -> usize {
    match tag.attributes.get("colspan") {
        Some(Scalar::Number(n)) if *n >= 1.0 => *n as usize,
        _ => 1,
    }
}

/// A cell's row span from its `rowspan` attribute; at least 1.
fn cell_rowspan(tag: &Tag) -> usize {
    match tag.attributes.get("rowspan") {
        Some(Scalar::Number(n)) if *n >= 1.0 => *n as usize,
        _ => 1,
    }
}

/// Table column count under the occupancy model: a `colspan` widens its row,
/// and a `rowspan` reserves columns for the following rows.
fn grid_num_cols(header_rows: &[RowSource<'_>], body_rows: &[RowSource<'_>]) -> usize {
    let mut occupied: Vec<usize> = Vec::new();
    let mut num_cols = 0usize;
    for src in header_rows.iter().chain(body_rows.iter()) {
        let mut col = 0usize;
        for cell in src.cells() {
            while col < occupied.len() && occupied[col] > 0 {
                col += 1;
            }
            let span = cell_colspan(cell);
            let rowspan = cell_rowspan(cell);
            while occupied.len() < col + span {
                occupied.push(0);
            }
            for o in occupied.iter_mut().take(col + span).skip(col) {
                *o = rowspan;
            }
            col += span;
        }
        num_cols = num_cols.max(occupied.len()).max(col);
        for o in occupied.iter_mut() {
            *o = o.saturating_sub(1);
        }
    }
    num_cols
}

/// A cell's own alignment override from its `align` attribute (set by the
/// list-syntax `{% align %}` annotation), if any.
fn cell_align(cell: &Tag) -> Option<parley::layout::Alignment> {
    use parley::layout::Alignment;
    match cell.attributes.get("align") {
        Some(Scalar::String(s)) => match s.as_str() {
            "center" => Some(Alignment::Center),
            "right" => Some(Alignment::End),
            "left" => Some(Alignment::Start),
            _ => None,
        },
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_row_source(
    src: &RowSource<'_>,
    is_header: bool,
    column_xs: &[f32],
    column_widths: &[f32],
    padding: f32,
    header_column: bool,
    column_aligns: &[parley::layout::Alignment],
    occupied: &mut [usize],
    ctx: &mut LayoutCtx<'_>,
) -> TableRow {
    let ncols = column_xs.len();
    let mut cells: Vec<TableCell> = Vec::new();
    let mut col = 0usize;
    for cell_tag in src.cells() {
        // Skip columns still held by a `rowspan` from an earlier row.
        while col < ncols && occupied.get(col).copied().unwrap_or(0) > 0 {
            col += 1;
        }
        if col >= ncols {
            break;
        }
        let span = cell_colspan(cell_tag).clamp(1, ncols - col);
        let rowspan = cell_rowspan(cell_tag).max(1);
        let last = col + span - 1;
        let cell_x = column_xs[col] + padding;
        // The merged cell spans from its first column's content start to its
        // last column's content end (covering the suppressed internal borders).
        let spanned_w = (column_xs[last] + column_widths[last]) - column_xs[col];
        let cell_text_width = (spanned_w - 2.0 * padding).max(0.0);
        // A header column makes column 0 a header cell even in body rows.
        let cell_is_header = is_header || (header_column && col == 0);
        // A cell's own `{% align %}` overrides the column alignment.
        let align = cell_align(cell_tag).unwrap_or_else(|| {
            column_aligns
                .get(col)
                .copied()
                .unwrap_or(parley::layout::Alignment::Start)
        });
        let blocks = layout_cell_content(
            cell_tag,
            cell_x,
            cell_text_width,
            cell_is_header,
            align,
            ctx,
        );
        // Reserve this cell's columns for the rows it spans.
        for o in occupied.iter_mut().take(last + 1).skip(col) {
            *o = rowspan;
        }
        cells.push(TableCell {
            blocks,
            col,
            colspan: span,
            rowspan,
        });
        col += span;
    }
    // Pad any remaining *uncovered* columns with empty single cells.
    while col < ncols {
        if occupied.get(col).copied().unwrap_or(0) == 0 {
            cells.push(TableCell {
                blocks: Vec::new(),
                col,
                colspan: 1,
                rowspan: 1,
            });
        }
        col += 1;
    }
    // The row's height comes from cells that DON'T span into later rows; a
    // rowspan cell's content extends downward instead of inflating this row.
    let mut max_height = cells
        .iter()
        .filter(|c| c.rowspan <= 1)
        .map(|c| sum_block_height(&c.blocks))
        .fold(0.0_f32, f32::max);
    if max_height == 0.0 {
        // A row of only rowspan cells — fall back to their height.
        max_height = cells
            .iter()
            .map(|c| sum_block_height(&c.blocks))
            .fold(0.0_f32, f32::max);
    }
    // Age the grid: each active rowspan covers one fewer following row.
    for o in occupied.iter_mut() {
        *o = o.saturating_sub(1);
    }
    TableRow {
        is_header,
        fill: None,
        header_column,
        height: max_height + 2.0 * padding,
        cells,
    }
}

/// Split `inner_width` across columns in the given relative `weights`
/// (e.g. `[1.0, 3.0]` → 25% / 75% of the width). Returns `None` when the
/// weights are unusable (empty, or any non-positive), so the caller falls
/// back to an automatic sizing mode.
fn weighted_column_widths(weights: &[f32], inner_width: f32) -> Option<Vec<f32>> {
    if weights.is_empty() || weights.iter().any(|w| *w <= 0.0) {
        return None;
    }
    let sum: f32 = weights.iter().sum();
    Some(weights.iter().map(|w| inner_width * w / sum).collect())
}

/// Two-pass auto-sizing: measure each cell's natural & min widths,
/// then distribute `inner_width` across columns proportionally.
fn compute_auto_column_widths(
    header_rows: &[RowSource<'_>],
    body_rows: &[RowSource<'_>],
    num_cols: usize,
    inner_width: f32,
    padding: f32,
    ctx: &mut LayoutCtx<'_>,
) -> Vec<f32> {
    let mut col_min: Vec<f32> = vec![0.0; num_cols];
    let mut col_max: Vec<f32> = vec![0.0; num_cols];

    for src in header_rows.iter().chain(body_rows.iter()) {
        let mut col = 0usize;
        for cell_tag in src.cells() {
            if col >= num_cols {
                break;
            }
            let span = cell_colspan(cell_tag).clamp(1, num_cols - col);
            // A spanning cell informs no single column's width — the columns
            // are sized by single-column cells, and the span covers them.
            if span == 1 {
                let weight = if matches!(src, RowSource::CellsDirect(_)) {
                    700.0
                } else {
                    400.0
                };
                let style = TextStyle {
                    font_size: ctx.style.body_font_size,
                    font_weight: weight,
                    line_height: ctx.style.body_line_height,
                    color: ctx.style.text_color.into(),
                    font_families: ctx.body_families,
                    italic: false,
                };
                let (cell_min, cell_max) = measure_cell_widths(cell_tag, &style, ctx);
                col_min[col] = col_min[col].max(cell_min + 2.0 * padding);
                col_max[col] = col_max[col].max(cell_max + 2.0 * padding);
            }
            col += span;
        }
    }

    // A single over-wide unbreakable token (a long URL, part number, …)
    // would otherwise force its column's minimum to the full token width
    // and squeeze every other column to nothing. Since `overflow-wrap:
    // anywhere` lets such a token break, cap each column's minimum at an
    // equal share so it can't dominate; the token then wraps within
    // whatever width the column is given. Columns still take their
    // natural width when the table has room (handled by `distribute`).
    let fair_min = inner_width / num_cols as f32;
    for m in &mut col_min {
        *m = m.min(fair_min);
    }

    distribute(col_min, col_max, inner_width)
}

/// Measure a cell's content widths via the inline-text projection.
/// Returns `(longest-word-width, full-natural-width)`.
fn measure_cell_widths(cell: &Tag, style: &TextStyle<'_>, ctx: &mut LayoutCtx<'_>) -> (f32, f32) {
    let mut text = String::new();
    let mut ranges = Vec::new();
    // Measurement only — drop any footnote calls into a throwaway
    // registry so column-width measurement doesn't allocate footnote
    // numbers that the actual layout pass would re-allocate.
    collect_inlines(&mut text, &mut ranges, &cell.children, &mut Vec::new());
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return (0.0, 0.0);
    }
    let natural = measure_first_line_width(trimmed, style, ctx.font_cx, ctx.layout_cx);
    let min_word = trimmed
        .split_whitespace()
        .map(|w| measure_first_line_width(w, style, ctx.font_cx, ctx.layout_cx))
        .fold(0.0_f32, f32::max);
    (min_word, natural)
}

/// Distribute `inner_width` between columns so that no column is
/// narrower than its `min_word` (so words don't get broken) but all
/// available space is consumed when content is narrower than the page.
fn distribute(col_min: Vec<f32>, col_max: Vec<f32>, inner_width: f32) -> Vec<f32> {
    let n = col_min.len();
    let sum_max: f32 = col_max.iter().sum();
    if sum_max <= inner_width {
        // Plenty of room: hand out a proportional share of the leftover
        // so the table stretches to fill (avoids ragged-right look).
        let leftover = inner_width - sum_max;
        return col_max
            .iter()
            .map(|m| m + leftover * (m / sum_max.max(1e-6)))
            .collect();
    }
    let sum_min: f32 = col_min.iter().sum();
    if sum_min >= inner_width {
        // Even minimums overflow: hand out proportional shares of mins.
        let scale = if sum_min > 0.0 {
            inner_width / sum_min
        } else {
            1.0
        };
        return col_min.iter().map(|m| m * scale).collect();
    }
    let extra = inner_width - sum_min;
    let total_diff: f32 = col_max
        .iter()
        .zip(&col_min)
        .map(|(a, b)| (a - b).max(0.0))
        .sum();
    if total_diff <= 0.0 {
        return vec![inner_width / n as f32; n];
    }
    col_min
        .iter()
        .zip(col_max.iter())
        .map(|(mn, mx)| mn + extra * ((mx - mn).max(0.0) / total_diff))
        .collect()
}

fn sum_block_height(blocks: &[Block]) -> f32 {
    let total: f32 = blocks.iter().map(|b| b.height + b.space_after).sum();
    total - blocks.last().map(|b| b.space_after).unwrap_or(0.0)
}

/// Lay out the contents of one cell. If the cell has only inline content,
/// treat it as a single paragraph (matches markdown table convention).
/// Header cells render with bold text and the configured header colour.
fn layout_cell_content(
    cell: &Tag,
    x: f32,
    width: f32,
    is_header: bool,
    align: parley::layout::Alignment,
    ctx: &mut LayoutCtx<'_>,
) -> Vec<Block> {
    // If there are no block-level children, render directly as a paragraph
    // so we can apply header bold + colour without nesting.
    let any_block_child = cell.children.iter().any(|c| {
        if let RenderableTreeNode::Tag(t) = c {
            matches!(
                t.name.as_str(),
                "p" | "ul"
                    | "ol"
                    | "list"
                    | "blockquote"
                    | "pre"
                    | "callout"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "table"
                    | "img"
                    | "media"
            )
        } else {
            false
        }
    });
    if !any_block_child {
        return layout_table_cell_paragraph(cell, x, width, is_header, align, ctx);
    }
    layout_children(&cell.children, x, width, ctx)
}

// ── Media (img / media) ─────────────────────────────────────────────────

fn layout_media(tag: &Tag, x: f32, width: f32, ctx: &mut LayoutCtx<'_>) -> Vec<Block> {
    // Source resolution. Either an explicit `src` (markdown `![]()` or
    // `{% media src=… %}`), or an Arca `id` (`{% media id="<uuid>" /%}`).
    // In production Scriptor rewrites `id` to a concrete `src` before this
    // runs; locally — the tech-writer workflow while Arca isn't ready — we
    // resolve the id ourselves by probing `<id>.<ext>` under the assets
    // root for the extensions we can decode (content is re-sniffed below,
    // so a wrong extension still works). `src` wins when both are present.
    let src_attr = tag.attributes.get("src").and_then(|v| match v {
        Scalar::String(s) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    });
    let id_attr = tag.attributes.get("id").and_then(|v| match v {
        Scalar::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    });
    let candidates: Vec<String> = if let Some(s) = &src_attr {
        vec![s.clone()]
    } else if let Some(id) = &id_attr {
        const EXTS: [&str; 6] = ["webp", "png", "jpg", "jpeg", "gif", "svg"];
        let mut v: Vec<String> = EXTS.iter().map(|e| format!("{id}.{e}")).collect();
        v.push(id.clone()); // bare id: already-extensioned or extensionless file
        v
    } else {
        return placeholder(x, width, ctx, "[media: missing src or id]");
    };

    // Alt text:
    //   - `{% media alt="..." /%}` exposes it as an attribute.
    //   - Markdown `![alt](url)` puts the alt as the Image node's
    //     children (Text scalars), since cmark only models alt-via-children.
    //   Fall through both sources.
    let alt = match tag.attributes.get("alt") {
        Some(Scalar::String(s)) if !s.is_empty() => s.clone(),
        _ => {
            let mut buf = String::new();
            for child in &tag.children {
                if let RenderableTreeNode::Scalar(Scalar::String(s)) = child {
                    buf.push_str(s);
                }
            }
            buf
        }
    };

    // Advisory `title` (HTML tooltip / markdown `![alt](url "title")`). Used
    // as a caption / accessibility fallback only when the style opts in via
    // `image_title_fallback`; otherwise it has no print rendering.
    let title = match tag.attributes.get("title") {
        Some(Scalar::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    };
    let use_title = ctx.style.image_title_fallback;

    // An explicit `caption="…"` attribute draws a visible caption block (like
    // a preceding `{% caption %}` tag) and also feeds the List of Figures /
    // the PDF/UA figure label. `had_caption_tag` records whether a
    // `{% caption %}` tag already produced a visible caption for this media
    // (handled in `layout_children`), so `caption=` doesn't double it.
    let caption_attr = match tag.attributes.get("caption") {
        Some(Scalar::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    };
    let had_caption_tag = ctx.pending_figure_caption.is_some();

    // `size` keyword scales the display width relative to the space available
    // here (a column, a table cell, …): small = 50 %, medium = 75 %, large =
    // 100 %. The default (no attribute) is 75 %. `fit_size` never upscales,
    // so a small source image still renders at its natural size.
    let avail = match tag.attributes.get("size") {
        Some(Scalar::String(s)) => match s.trim() {
            "small" => width * 0.5,
            "medium" => width * 0.75,
            "large" => width,
            _ => width * 0.75,
        },
        _ => width * 0.75,
    };

    // Fetch the first candidate that resolves. A missing file fails the
    // open immediately (no bytes read), so probing extensions is cheap.
    let (src, bytes) = {
        let mut found: Option<(String, Vec<u8>)> = None;
        let mut last_err: Option<String> = None;
        for cand in &candidates {
            match ctx.assets.fetch(cand) {
                Ok(b) => {
                    found = Some((cand.clone(), b));
                    break;
                }
                Err(e) => last_err = Some(e.to_string()),
            }
        }
        // Flat probe missed. If this was an `id`, search the assets tree
        // recursively for a file whose stem matches (nested asset layouts).
        if found.is_none()
            && let Some(id) = &id_attr
            && let Some(path) = ctx.assets.resolve_id(id.as_str())
            && let Ok(b) = ctx.assets.fetch(&path)
        {
            found = Some((path, b));
        }
        match found {
            Some(fb) => fb,
            None => {
                // Loud-on-stderr so a typo'd path / unknown id doesn't slip
                // past a tech writer iterating locally.
                let what = src_attr
                    .clone()
                    .or_else(|| id_attr.clone())
                    .unwrap_or_default();
                let err = last_err.unwrap_or_else(|| "not found".to_string());
                eprintln!("warning: media unavailable: {what} — {err}");
                let msg = if alt.is_empty() {
                    format!("[media unavailable: {what}]")
                } else {
                    format!("[{alt}]")
                };
                return placeholder(x, width, ctx, &msg);
            }
        }
    };

    let format = sniff_format(&bytes);
    match format {
        MediaFormat::Png | MediaFormat::Jpeg | MediaFormat::Gif | MediaFormat::Webp => {
            let image = match format {
                MediaFormat::Png => KrillaImage::from_png(bytes.into(), false),
                MediaFormat::Jpeg => KrillaImage::from_jpeg(bytes.into(), false),
                MediaFormat::Gif => KrillaImage::from_gif(bytes.into(), false),
                MediaFormat::Webp => KrillaImage::from_webp(bytes.into(), false),
                _ => unreachable!(),
            };
            let image = match image {
                Ok(img) => img,
                Err(e) => {
                    eprintln!("warning: media decode failed: {src} — {e}");
                    return placeholder(
                        x,
                        width,
                        ctx,
                        &format!("[media decode failed: {src} — {e}]"),
                    );
                }
            };
            let (px_w, px_h) = image.size();
            let (display_w, display_h) = fit_size(px_w as f32, px_h as f32, avail);
            let draw_x = align_within(x, width, display_w, ctx.cell_content_align);
            let figure_id = ctx.next_figure_id();
            // Explicit `{% caption %}` wins over `caption=`, then alt / title.
            let caption = ctx
                .pending_figure_caption
                .take()
                .or_else(|| caption_attr.clone())
                .or_else(|| (!alt.is_empty()).then(|| alt.clone()))
                .or_else(|| if use_title { title.clone() } else { None });
            let block = Block {
                height: display_h,
                space_after: ctx.style.paragraph_space_after,
                draw: BlockDraw::Image {
                    image,
                    x: draw_x,
                    width: display_w,
                    height: display_h,
                    caption,
                },
                outline: None,
                anchor_id: Some(format!("__figure_{figure_id}")),
                tag_role: None,
                page_column: 0,
                column_span: false,
            };
            let visible = if had_caption_tag { None } else { caption_attr };
            finish_media(block, visible, x, width, ctx)
        }
        MediaFormat::Svg => {
            // Build a usvg::Tree. The tree carries its own font database;
            // for now we use the default options (no system font lookup
            // for SVG-embedded text). For text-heavy SVGs, callers can
            // pre-process upstream.
            let opts = usvg::Options::default();
            match usvg::Tree::from_data(&bytes, &opts) {
                Ok(tree) => {
                    let size = tree.size();
                    let (display_w, display_h) = fit_size(size.width(), size.height(), avail);
                    let draw_x = align_within(x, width, display_w, ctx.cell_content_align);
                    let figure_id = ctx.next_figure_id();
                    let caption = ctx
                        .pending_figure_caption
                        .take()
                        .or_else(|| caption_attr.clone())
                        .or_else(|| (!alt.is_empty()).then(|| alt.clone()))
                        .or_else(|| if use_title { title.clone() } else { None });
                    let block = Block {
                        height: display_h,
                        space_after: ctx.style.paragraph_space_after,
                        draw: BlockDraw::Svg {
                            tree: Arc::new(tree),
                            x: draw_x,
                            width: display_w,
                            height: display_h,
                            caption,
                        },
                        outline: None,
                        anchor_id: Some(format!("__figure_{figure_id}")),
                        tag_role: None,
                        page_column: 0,
                        column_span: false,
                    };
                    let visible = if had_caption_tag { None } else { caption_attr };
                    finish_media(block, visible, x, width, ctx)
                }
                Err(e) => {
                    eprintln!("warning: svg parse failed: {src} — {e}");
                    placeholder(x, width, ctx, &format!("[svg parse failed: {src} — {e}]"))
                }
            }
        }
        MediaFormat::Unknown => {
            eprintln!(
                "warning: unknown media format: {src} (sniffed bytes don't match png/jpeg/gif/webp/svg)"
            );
            placeholder(x, width, ctx, &format!("[unknown media format: {src}]"))
        }
    }
}

/// Shift `x` so a `content`-wide box sits centre / right within an
/// `avail`-wide slot, per the transient cell alignment. `None` / `Start`
/// leave it at the left edge; anything wider than the slot stays put.
fn align_within(x: f32, avail: f32, content: f32, align: Option<parley::layout::Alignment>) -> f32 {
    use parley::layout::Alignment;
    let slack = (avail - content).max(0.0);
    match align {
        Some(Alignment::Center) => x + slack * 0.5,
        Some(Alignment::End) => x + slack,
        _ => x,
    }
}

/// Fit `(natural_w, natural_h)` into `available_w` preserving aspect.
/// Never upscales — small images render at their natural size.
fn fit_size(natural_w: f32, natural_h: f32, available_w: f32) -> (f32, f32) {
    if natural_w <= 0.0 || natural_h <= 0.0 {
        return (available_w, available_w * 0.5);
    }
    if natural_w <= available_w {
        return (natural_w, natural_h);
    }
    let scale = available_w / natural_w;
    (available_w, natural_h * scale)
}

/// Render a small text placeholder when we can't display the actual
/// media. Used for missing files, decode failures, unsupported schemes.
fn placeholder(x: f32, width: f32, ctx: &mut LayoutCtx<'_>, message: &str) -> Vec<Block> {
    let style = TextStyle {
        font_size: ctx.style.body_font_size,
        font_weight: 400.0,
        line_height: ctx.style.body_line_height,
        color: ctx.style.blockquote_text_color.into(),
        font_families: ctx.body_families,
        italic: false,
    };
    let text = message.to_string();
    let layout = build_layout(&text, &[], &style, width, ctx.font_cx, ctx.layout_cx);
    let slice = TextSlice::whole(layout, text, Vec::new(), x);
    let height = slice.height();
    vec![Block {
        height,
        space_after: ctx.style.paragraph_space_after,
        draw: BlockDraw::Text(slice),
        outline: None,
        anchor_id: None,

        tag_role: None,
        page_column: 0,
        column_span: false,
    }]
}

fn layout_table_cell_paragraph(
    cell: &Tag,
    x: f32,
    width: f32,
    is_header: bool,
    align: parley::layout::Alignment,
    ctx: &mut LayoutCtx<'_>,
) -> Vec<Block> {
    // Table cells don't carry document footnotes for v1 — the
    // pagination pool only attaches to top-level body blocks, so
    // routing cell footnotes there could end up on the wrong page.
    let inlines = Inlines::from_with_labels(
        &cell.children,
        &mut Vec::new(),
        Some(ctx.crossref_labels),
    );
    if inlines.text.trim().is_empty() {
        return Vec::new();
    }
    let (weight, color) = if is_header {
        (700.0, ctx.style.table_header_text_color.into())
    } else {
        (400.0, ctx.style.text_color.into())
    };
    let style = TextStyle {
        font_size: ctx.style.body_font_size,
        font_weight: weight,
        line_height: ctx.style.body_line_height,
        color,
        font_families: ctx.body_families,
        italic: false,
    };
    let layout = build_layout_aligned(
        &inlines.text,
        &inlines.style_ranges,
        &style,
        width,
        align,
        ctx.font_cx,
        ctx.layout_cx,
    );
    let slice = TextSlice::whole(layout, inlines.text, inlines.links, x);
    let height = slice.height();
    vec![Block {
        height,
        space_after: 0.0,
        draw: BlockDraw::Text(slice),
        outline: None,
        anchor_id: None,

        tag_role: None,
        page_column: 0,
        column_span: false,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use markdoc::types::Tag as MdTag;
    use std::collections::HashMap;

    #[test]
    fn heading_counters_nest_and_reset() {
        let mut c = [0u32; 6];
        // h1, h2, h3 walk down.
        assert_eq!(bump_heading_counters(&mut c, 1, 3).as_deref(), Some("1"));
        assert_eq!(bump_heading_counters(&mut c, 2, 3).as_deref(), Some("1.1"));
        assert_eq!(
            bump_heading_counters(&mut c, 3, 3).as_deref(),
            Some("1.1.1")
        );
        // Another h3 increments only the deepest.
        assert_eq!(
            bump_heading_counters(&mut c, 3, 3).as_deref(),
            Some("1.1.2")
        );
        // A new h2 resets the h3 counter.
        assert_eq!(bump_heading_counters(&mut c, 2, 3).as_deref(), Some("1.2"));
        assert_eq!(
            bump_heading_counters(&mut c, 3, 3).as_deref(),
            Some("1.2.1")
        );
        // A new h1 resets h2 and h3.
        assert_eq!(bump_heading_counters(&mut c, 1, 3).as_deref(), Some("2"));
        assert_eq!(bump_heading_counters(&mut c, 2, 3).as_deref(), Some("2.1"));
    }

    #[test]
    fn ordered_marker_sequences_format() {
        use super::super::style::MarkerSequence::*;
        assert_eq!(format_ordered_marker(1, Decimal), "1");
        assert_eq!(format_ordered_marker(42, Decimal), "42");
        // Bijective base-26.
        assert_eq!(format_ordered_marker(1, LowerAlpha), "a");
        assert_eq!(format_ordered_marker(26, LowerAlpha), "z");
        assert_eq!(format_ordered_marker(27, LowerAlpha), "aa");
        assert_eq!(format_ordered_marker(28, LowerAlpha), "ab");
        assert_eq!(format_ordered_marker(2, UpperAlpha), "B");
        // Roman numerals.
        assert_eq!(format_ordered_marker(4, LowerRoman), "iv");
        assert_eq!(format_ordered_marker(9, LowerRoman), "ix");
        assert_eq!(format_ordered_marker(2026, LowerRoman), "mmxxvi");
        assert_eq!(format_ordered_marker(14, UpperRoman), "XIV");
        // Out-of-range roman falls back to decimal.
        assert_eq!(format_ordered_marker(4000, LowerRoman), "4000");
    }

    #[test]
    fn overwide_token_does_not_squeeze_other_columns() {
        // A 2-column table whose column 1 holds an unbreakable token far
        // wider than the page (min == max == 800pt) while column 0 needs
        // only 30pt. Without capping, the proportional min-scaling starves
        // column 0 below its natural width (the "FieldValue" squeeze).
        let starved = distribute(vec![30.0, 800.0], vec![30.0, 800.0], 470.0);
        assert!(starved[0] < 25.0, "col 0 starved: {}", starved[0]);
        // Capping column 1's minimum at the fair share (470/2 = 235pt)
        // lets the token wrap (overflow-wrap: anywhere) so column 0 keeps
        // its natural 30pt and column 1 absorbs the rest.
        let ok = distribute(vec![30.0, 235.0], vec![30.0, 800.0], 470.0);
        assert!((ok[0] - 30.0).abs() < 0.5, "col 0 width: {}", ok[0]);
        assert!(ok[1] > 400.0, "col 1 width: {}", ok[1]);
    }

    #[test]
    fn blank_cell_detection_drops_empty_headers() {
        let txt = |s: &str| RenderableTreeNode::Scalar(Scalar::String(s.to_string()));
        let tag = |name: &str, children: Vec<RenderableTreeNode>| {
            RenderableTreeNode::Tag(Box::new(MdTag {
                name: name.to_string(),
                attributes: HashMap::new(),
                children,
            }))
        };
        // Empty / whitespace-only content (directly or nested) is blank —
        // the markdown `| | |` header case.
        assert!(node_text_is_blank(&[]));
        assert!(node_text_is_blank(&[txt("   ")]));
        assert!(node_text_is_blank(&[tag("p", vec![txt("")])]));
        // Visible text — directly or nested — is not blank.
        assert!(!node_text_is_blank(&[txt("Date")]));
        assert!(!node_text_is_blank(&[tag("strong", vec![txt("To:")])]));
        // An image/media tag counts as visible even with no text.
        assert!(!node_text_is_blank(&[tag("img", vec![])]));
    }

    #[test]
    fn heading_numbering_respects_max_depth() {
        let mut c = [0u32; 6];
        assert_eq!(bump_heading_counters(&mut c, 1, 2).as_deref(), Some("1"));
        assert_eq!(bump_heading_counters(&mut c, 2, 2).as_deref(), Some("1.1"));
        // h3 is past max_depth = 2 → no number, and counters untouched.
        assert_eq!(bump_heading_counters(&mut c, 3, 2), None);
        // The next h2 still follows on from 1.1, unaffected by the h3.
        assert_eq!(bump_heading_counters(&mut c, 2, 2).as_deref(), Some("1.2"));
    }

    #[test]
    fn heading_numbering_clamps_and_guards() {
        let mut c = [0u32; 6];
        // level 0 never numbers.
        assert_eq!(bump_heading_counters(&mut c, 0, 3), None);
        // max_depth 0 clamps up to 1 so h1 still numbers.
        assert_eq!(bump_heading_counters(&mut c, 1, 0).as_deref(), Some("1"));
    }

    fn tag_with_attrs(attrs: &[(&str, Scalar)]) -> MdTag {
        let mut m = HashMap::new();
        for (k, v) in attrs {
            m.insert((*k).to_string(), v.clone());
        }
        MdTag {
            name: "tag".to_string(),
            attributes: m,
            children: Vec::new(),
        }
    }

    #[test]
    fn attr_is_false_recognises_falsey_forms() {
        assert!(attr_is_false(
            &tag_with_attrs(&[("numbered", Scalar::Boolean(false))]),
            "numbered"
        ));
        assert!(attr_is_false(
            &tag_with_attrs(&[("numbered", Scalar::String("false".into()))]),
            "numbered"
        ));
        assert!(attr_is_false(
            &tag_with_attrs(&[("numbered", Scalar::String("False".into()))]),
            "numbered"
        ));
        assert!(attr_is_false(
            &tag_with_attrs(&[("numbered", Scalar::String("0".into()))]),
            "numbered"
        ));
    }

    #[test]
    fn attr_is_false_rejects_truthy_or_absent() {
        assert!(!attr_is_false(
            &tag_with_attrs(&[("numbered", Scalar::Boolean(true))]),
            "numbered"
        ));
        assert!(!attr_is_false(
            &tag_with_attrs(&[("numbered", Scalar::String("true".into()))]),
            "numbered"
        ));
        // Absent attribute → not explicitly false.
        assert!(!attr_is_false(&tag_with_attrs(&[]), "numbered"));
    }

    #[test]
    fn weighted_column_widths_split_in_proportion() {
        // [1, 3] over 400pt → 100 / 300.
        let ws = weighted_column_widths(&[1.0, 3.0], 400.0).expect("usable weights");
        assert_eq!(ws, vec![100.0, 300.0]);
        // Weights are relative, not absolute — [2, 6] gives the same split.
        let ws2 = weighted_column_widths(&[2.0, 6.0], 400.0).expect("usable weights");
        assert_eq!(ws2, vec![100.0, 300.0]);
        // Unusable inputs return None so the caller falls back to auto sizing.
        assert!(weighted_column_widths(&[], 400.0).is_none());
        assert!(weighted_column_widths(&[1.0, 0.0], 400.0).is_none());
        assert!(weighted_column_widths(&[1.0, -2.0], 400.0).is_none());
    }

    #[test]
    fn table_override_parses_tag_attributes() {
        use crate::render::style::TableBorders;
        use std::collections::HashMap;
        let mut a = HashMap::new();
        a.insert("borders".to_string(), Scalar::String("horizontal".into()));
        a.insert(
            "header_background".to_string(),
            Scalar::String("#e8e8e8".into()),
        );
        a.insert("cell_padding".to_string(), Scalar::Number(6.0));
        a.insert("column_weights".to_string(), Scalar::String("1 3.5".into()));
        a.insert("stripe".to_string(), Scalar::String("#f5f5f5".into()));
        a.insert("header_column".to_string(), Scalar::Boolean(true));
        let ov = TableOverride::from_attrs(&a);
        assert_eq!(ov.borders, Some(TableBorders::Horizontal));
        assert!(ov.header_bg.is_some());
        assert_eq!(ov.cell_padding, Some(6.0));
        assert_eq!(ov.column_weights, Some(vec![1.0, 3.5]));
        assert!(matches!(ov.stripe, Some(Some(_))));
        assert_eq!(ov.header_column, Some(true));

        // `stripe="none"` is explicit-off (distinct from "inherit").
        let mut b = HashMap::new();
        b.insert("stripe".to_string(), Scalar::String("none".into()));
        assert!(matches!(TableOverride::from_attrs(&b).stripe, Some(None)));

        // No attributes → inherit everything (all None).
        let none = TableOverride::from_attrs(&HashMap::new());
        assert!(none.borders.is_none());
        assert!(none.stripe.is_none());
        assert!(none.column_weights.is_none());
        assert!(none.header_bg.is_none());
    }
}
