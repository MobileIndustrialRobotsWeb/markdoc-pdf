//! Emission pass: draw a sequence of `Block`s to a krilla surface.

use std::collections::HashMap;

use krilla::color::rgb;
use krilla::geom::{PathBuilder, Point, Size, Transform};
use krilla::num::NormalizedF32;
use krilla::paint::{Fill, Stroke};
use krilla::tagging::{
    Artifact, ArtifactType, ContentTag, Identifier, Node as TagNode, SpanTag, Tag, TagGroup,
    TagKind, kind,
};
use krilla::text::Font;
use krilla_svg::{SurfaceExt, SvgSettings};
use parley::Layout;

use super::block::{Block, BlockDraw};
use super::inline::LinkRange;
use super::style::TableBorders;
use super::text::{emit_layout, emit_layout_segmented};

/// Multi-column body layout parameters for the emit pass.
#[derive(Debug, Clone, Copy)]
pub struct PageColumnLayout {
    pub columns: u8,
    pub column_width: f32,
    pub gap: f32,
}

impl PageColumnLayout {
    pub fn x_offset_for(&self, block: &Block) -> f32 {
        if self.columns <= 1 || block.column_span {
            0.0
        } else {
            block.page_column as f32 * (self.column_width + self.gap)
        }
    }
}

/// `(x, y, width, height)` for one line of a link's bounding region,
/// in page-local coordinates with y growing downward.
pub type LineRect = (f32, f32, f32, f32);

/// A page-local link annotation collected during emission. Converted
/// to a krilla `Annotation` by the caller and added to the page.
///
/// One `DeferredLink` represents the full link, even when its text
/// wraps onto several lines. `rects` lists the per-line bounding
/// rectangles in reading order; the caller folds them into one
/// annotation with `quad_points` so wrapped links produce a single
/// hot region (and a single PDF/UA `Link` tag group) rather than one
/// annotation per line.
///
/// `text_segment_ids` is non-empty when the document is being rendered
/// with PDF/UA tagging on — one identifier per `rects` entry, in the
/// same order. The caller stitches them with the annotation identifier
/// returned by `Page::add_tagged_annotation` into the `Link` tag group.
pub struct DeferredLink {
    pub rects: Vec<LineRect>,
    pub href: String,
    pub alt: Option<String>,
    pub text_segment_ids: Vec<Identifier>,
}

/// A heading anchor collected during emission. Converted to a krilla
/// `OutlineNode` by the caller, with the page index supplied externally.
pub struct OutlinePoint {
    pub level: u8,
    pub text: String,
    /// Y position (top of the heading) in page-local coords.
    pub y: f32,
}

/// Per-emit accumulator for tagged-PDF structure-tree fragments. Uses
/// a stack so nested groups (`L` → `LI` → `LBody`, `Table` → `TR` →
/// `TD`) build incrementally as emit walks blocks.
///
/// - `enter(group)` opens a new group on the stack.
/// - `leave()` pops the topmost group and attaches it to its parent
///   (or `nodes` when the stack is empty).
/// - `push_leaf(node)` attaches a leaf (e.g. a P group containing one
///   Span identifier) to the current parent, no nesting.
pub struct TagAccumulator {
    pub nodes: Vec<TagNode>,
    pub enabled: bool,
    stack: Vec<TagGroup>,
}

impl TagAccumulator {
    pub fn new(enabled: bool) -> Self {
        Self {
            nodes: Vec::new(),
            enabled,
            stack: Vec::new(),
        }
    }

    pub fn enter(&mut self, group: TagGroup) {
        if self.enabled {
            self.stack.push(group);
        }
    }

    pub fn leave(&mut self) {
        if !self.enabled {
            return;
        }
        if let Some(group) = self.stack.pop() {
            self.attach_node(group.into());
        }
    }

    pub fn push_leaf(&mut self, node: TagNode) {
        if !self.enabled {
            return;
        }
        self.attach_node(node);
    }

    fn attach_node(&mut self, node: TagNode) {
        if let Some(top) = self.stack.last_mut() {
            top.push(node);
        } else {
            self.nodes.push(node);
        }
    }
}

/// Draw `blocks` starting at top y = `start_y`. Any link annotations
/// produced by Text blocks are appended to `links`. Returns the bottom
/// y after all blocks have been drawn (max column bottom when multi-column).
pub fn emit_blocks(
    surface: &mut krilla::surface::Surface<'_>,
    blocks: &[Block],
    start_y: f32,
    font_cache: &mut HashMap<u64, Font>,
    links: &mut Vec<DeferredLink>,
    outline: &mut Vec<OutlinePoint>,
    tags: &mut TagAccumulator,
    columns: Option<PageColumnLayout>,
) -> f32 {
    if columns.map(|c| c.columns).unwrap_or(1) <= 1 {
        return emit_blocks_single(
            surface, blocks, start_y, font_cache, links, outline, tags, 0.0,
        );
    }
    emit_blocks_multi(
        surface,
        blocks,
        start_y,
        font_cache,
        links,
        outline,
        tags,
        columns.unwrap(),
    )
}

fn emit_blocks_single(
    surface: &mut krilla::surface::Surface<'_>,
    blocks: &[Block],
    start_y: f32,
    font_cache: &mut HashMap<u64, Font>,
    links: &mut Vec<DeferredLink>,
    outline: &mut Vec<OutlinePoint>,
    tags: &mut TagAccumulator,
    x_offset: f32,
) -> f32 {
    use krilla::tagging::ListNumbering;
    let mut y = start_y;
    let mut i = 0;
    while i < blocks.len() {
        // Group consecutive ListItem blocks of the same ordered/unordered
        // kind into one L tag for PDF/UA. Mixed runs (e.g. <ol> followed
        // by a <ul>) split into separate L groups so the /ListNumbering
        // attribute on each matches its actual marker style.
        let head_ordered = match &blocks[i].draw {
            BlockDraw::ListItem { ordered, .. } if tags.enabled => Some(*ordered),
            _ => None,
        };
        if let Some(head_ordered) = head_ordered {
            let numbering = if head_ordered {
                ListNumbering::Decimal
            } else {
                ListNumbering::Disc
            };
            tags.enter(TagGroup::new(TagKind::L(Tag::<kind::L>::L(numbering))));
            while i < blocks.len()
                && matches!(
                    &blocks[i].draw,
                    BlockDraw::ListItem { ordered, .. } if *ordered == head_ordered
                )
            {
                let block = &blocks[i];
                tags.enter(TagGroup::new(TagKind::LI(Tag::<kind::LI>::LI)));
                emit_block(
                    surface,
                    block,
                    y,
                    x_offset,
                    font_cache,
                    links,
                    outline,
                    tags,
                );
                tags.leave(); // LI
                y += block.height + block.space_after;
                i += 1;
            }
            tags.leave(); // L
            continue;
        }

        let block = &blocks[i];
        if let Some(entry) = &block.outline {
            outline.push(OutlinePoint {
                level: entry.level,
                text: entry.text.clone(),
                y,
            });
        }
        emit_block(
            surface,
            block,
            y,
            x_offset,
            font_cache,
            links,
            outline,
            tags,
        );
        y += block.height + block.space_after;
        i += 1;
    }
    y
}

fn emit_blocks_multi(
    surface: &mut krilla::surface::Surface<'_>,
    blocks: &[Block],
    start_y: f32,
    font_cache: &mut HashMap<u64, Font>,
    links: &mut Vec<DeferredLink>,
    outline: &mut Vec<OutlinePoint>,
    tags: &mut TagAccumulator,
    layout: PageColumnLayout,
) -> f32 {
    use krilla::tagging::ListNumbering;
    let n = layout.columns.max(1) as usize;
    let mut col_y = vec![start_y; n];
    let mut i = 0;
    while i < blocks.len() {
        let block = &blocks[i];
        let col_idx = block.page_column.min(layout.columns.saturating_sub(1)) as usize;
        let y = if block.column_span {
            col_y.iter().copied().fold(start_y, f32::max)
        } else {
            col_y[col_idx]
        };
        let x_offset = layout.x_offset_for(block);

        let head_ordered = match &block.draw {
            BlockDraw::ListItem { ordered, .. } if tags.enabled => Some(*ordered),
            _ => None,
        };
        if let Some(head_ordered) = head_ordered {
            let numbering = if head_ordered {
                ListNumbering::Decimal
            } else {
                ListNumbering::Disc
            };
            tags.enter(TagGroup::new(TagKind::L(Tag::<kind::L>::L(numbering))));
            let start_col = block.page_column;
            while i < blocks.len()
                && matches!(
                    &blocks[i].draw,
                    BlockDraw::ListItem { ordered, .. } if *ordered == head_ordered
                )
                && blocks[i].page_column == start_col
            {
                let block = &blocks[i];
                let col_idx =
                    block.page_column.min(layout.columns.saturating_sub(1)) as usize;
                let y = col_y[col_idx];
                let x_offset = layout.x_offset_for(block);
                tags.enter(TagGroup::new(TagKind::LI(Tag::<kind::LI>::LI)));
                emit_block(
                    surface,
                    block,
                    y,
                    x_offset,
                    font_cache,
                    links,
                    outline,
                    tags,
                );
                tags.leave();
                col_y[col_idx] += block.height + block.space_after;
                i += 1;
            }
            tags.leave();
            continue;
        }

        if let Some(entry) = &block.outline {
            outline.push(OutlinePoint {
                level: entry.level,
                text: entry.text.clone(),
                y,
            });
        }
        emit_block(
            surface,
            block,
            y,
            x_offset,
            font_cache,
            links,
            outline,
            tags,
        );
        let advance = block.height + block.space_after;
        if block.column_span {
            let new_y = y + advance;
            for cy in &mut col_y {
                *cy = new_y;
            }
        } else {
            col_y[col_idx] = y + advance;
        }
        i += 1;
    }
    col_y.into_iter().fold(start_y, f32::max)
}

fn emit_block(
    surface: &mut krilla::surface::Surface<'_>,
    block: &Block,
    y: f32,
    x_offset: f32,
    font_cache: &mut HashMap<u64, Font>,
    links: &mut Vec<DeferredLink>,
    outline: &mut Vec<OutlinePoint>,
    tags: &mut TagAccumulator,
) {
    if x_offset != 0.0 {
        surface.push_transform(&Transform::from_translate(x_offset, 0.0));
    }
    emit_block_inner(surface, block, y, font_cache, links, outline, tags);
    if x_offset != 0.0 {
        surface.pop();
    }
}

fn emit_block_inner(
    surface: &mut krilla::surface::Surface<'_>,
    block: &Block,
    y: f32,
    font_cache: &mut HashMap<u64, Font>,
    links: &mut Vec<DeferredLink>,
    outline: &mut Vec<OutlinePoint>,
    tags: &mut TagAccumulator,
) {
    match &block.draw {
        BlockDraw::Text(slice) => {
            // Structure-tree tag for this paragraph: a heading maps to
            // `H1`–`H6`, a note callout body to `Note`, everything else to
            // `P`. Computed here so the untagged path pays nothing.
            let kind: TagKind = if let Some(entry) = &block.outline {
                heading_tag_kind(entry.level, &entry.text)
            } else if matches!(block.tag_role, Some(super::block::TagRole::Note)) {
                TagKind::Note(Tag::<kind::Note>::Note)
            } else {
                TagKind::P(Tag::<kind::P>::P)
            };
            emit_text_slice(surface, slice, y, font_cache, links, tags, kind);
        }

        BlockDraw::Image {
            image,
            x,
            width,
            height,
            caption,
        } => {
            let size = match Size::from_wh(*width, *height) {
                Some(s) => s,
                None => return,
            };
            let id_opt = if tags.enabled {
                Some(surface.start_tagged(ContentTag::Other))
            } else {
                None
            };
            surface.push_transform(&Transform::from_translate(*x, y));
            surface.draw_image(image.clone(), size);
            surface.pop();
            if let Some(id) = id_opt {
                surface.end_tagged();
                let mut g = TagGroup::new(TagKind::Figure(Tag::<kind::Figure>::Figure(
                    caption.clone(),
                )));
                g.push(id);
                tags.push_leaf(g.into());
            }
        }

        BlockDraw::Svg {
            tree,
            x,
            width,
            height,
            caption,
        } => {
            let size = match Size::from_wh(*width, *height) {
                Some(s) => s,
                None => return,
            };
            let id_opt = if tags.enabled {
                Some(surface.start_tagged(ContentTag::Other))
            } else {
                None
            };
            surface.push_transform(&Transform::from_translate(*x, y));
            surface.draw_svg(tree.as_ref(), size, SvgSettings::default());
            surface.pop();
            if let Some(id) = id_opt {
                surface.end_tagged();
                let mut g = TagGroup::new(TagKind::Figure(Tag::<kind::Figure>::Figure(
                    caption.clone(),
                )));
                g.push(id);
                tags.push_leaf(g.into());
            }
        }

        BlockDraw::PageBreak => {
            // Marker — paginator already consumed it. Defensive no-op
            // so a stray instance can't crash emit.
        }

        BlockDraw::Rule {
            x,
            width,
            thickness,
            color,
        } => {
            if *thickness <= 0.0 || *width <= 0.0 {
                return; // spacer or no-op
            }
            let mut pb = PathBuilder::new();
            pb.move_to(*x, y + *thickness * 0.5);
            pb.line_to(*x + *width, y + *thickness * 0.5);
            let path = pb.finish().unwrap();
            surface.set_stroke(Some(Stroke {
                paint: (*color).into(),
                width: *thickness,
                opacity: NormalizedF32::ONE,
                ..Default::default()
            }));
            surface.draw_path(&path);
            // Krilla persists stroke state until the next set_stroke
            // call. Clearing it now means subsequent text emits don't
            // get drawn under text-rendering-mode 2 (fill + stroke).
            surface.set_stroke(None);
        }

        BlockDraw::BoxedGroup {
            x,
            width,
            background,
            border,
            accent_left,
            accent_width,
            padding,
            children,
            icon,
            top_rule,
            bottom_rule,
        } => {
            let box_top = y;
            let box_bottom = y + block.height;

            // 1. Background fill.
            if let Some(bg) = background {
                let mut pb = PathBuilder::new();
                pb.move_to(*x, box_top);
                pb.line_to(*x + *width, box_top);
                pb.line_to(*x + *width, box_bottom);
                pb.line_to(*x, box_bottom);
                pb.close();
                let path = pb.finish().unwrap();
                surface.set_fill(Some(Fill {
                    paint: (*bg).into(),
                    opacity: NormalizedF32::ONE,
                    rule: Default::default(),
                }));
                surface.draw_path(&path);
            }

            // 2. Border outline.
            if let Some(b) = border {
                let mut pb = PathBuilder::new();
                pb.move_to(*x, box_top);
                pb.line_to(*x + *width, box_top);
                pb.line_to(*x + *width, box_bottom);
                pb.line_to(*x, box_bottom);
                pb.close();
                let path = pb.finish().unwrap();
                surface.set_stroke(Some(Stroke {
                    paint: (*b).into(),
                    width: 0.5,
                    opacity: NormalizedF32::ONE,
                    ..Default::default()
                }));
                surface.draw_path(&path);
                surface.set_stroke(None);
            }

            // 3. Left accent stripe.
            if let Some(a) = accent_left {
                let mut pb = PathBuilder::new();
                pb.move_to(*x, box_top);
                pb.line_to(*x + *accent_width, box_top);
                pb.line_to(*x + *accent_width, box_bottom);
                pb.line_to(*x, box_bottom);
                pb.close();
                let path = pb.finish().unwrap();
                surface.set_fill(Some(Fill {
                    paint: (*a).into(),
                    opacity: NormalizedF32::ONE,
                    rule: Default::default(),
                }));
                surface.draw_path(&path);
            }

            // 3b. Bulletin rules across the top / bottom edges.
            let mut draw_edge_rule = |edge_y: f32, color: rgb::Color, thickness: f32| {
                let mut pb = PathBuilder::new();
                pb.move_to(*x, edge_y);
                pb.line_to(*x + *width, edge_y);
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
            };
            if let Some((color, thickness)) = top_rule {
                draw_edge_rule(box_top, *color, *thickness);
            }
            if let Some((color, thickness)) = bottom_rule {
                draw_edge_rule(box_bottom, *color, *thickness);
            }

            // 4. Optional icon at the box's top-left content corner.
            //    Marked as a decorative Artifact so assistive tech
            //    ignores it — the bold label carries the meaning. Drawn
            //    before the children; their x already clears the gutter.
            if let Some(icon) = icon
                && let Some(size) = Size::from_wh(icon.size, icon.size)
            {
                let icon_y = box_top + *padding;
                if tags.enabled {
                    surface.start_tagged(ContentTag::Artifact(Artifact::with_kind(
                        ArtifactType::Other,
                    )));
                }
                surface.push_transform(&Transform::from_translate(icon.x, icon_y));
                match &icon.decoded {
                    super::decoration::DecodedMedia::Raster(img) => {
                        surface.draw_image(img.clone(), size);
                    }
                    super::decoration::DecodedMedia::Svg(tree) => {
                        surface.draw_svg(tree.as_ref(), size, SvgSettings::default());
                    }
                }
                surface.pop();
                if tags.enabled {
                    surface.end_tagged();
                }
            }

            // 5. Children, offset by padding (children's x already
            //    accounts for layout-time x-shift; padding here shifts y).
            let _ = padding; // padding-x is baked into children x at layout time
            emit_blocks(
                surface,
                children,
                box_top + *padding,
                font_cache,
                links,
                outline,
                tags,
                None,
            );
        }

        BlockDraw::ListItem {
            marker,
            marker_text,
            marker_x,
            body,
            ordered: _,
            badge,
            check,
        } => {
            // When the marker is badged, draw its filled circle first
            // (so the glyphs sit on top) and centre the marker over the
            // circle; otherwise the marker is left-aligned at marker_x.
            let marker_origin_x = if let Some(b) = badge {
                let cx = *marker_x + b.center_dx;
                let cy = y + b.center_dy;
                // The circle is decorative — the marker glyph (tagged Lbl
                // below) carries the meaning — so mark it an Artifact.
                if tags.enabled {
                    surface.start_tagged(ContentTag::Artifact(Artifact::with_kind(
                        ArtifactType::Other,
                    )));
                }
                fill_circle(surface, cx, cy, b.diameter * 0.5, b.fill);
                if tags.enabled {
                    surface.end_tagged();
                }
                let marker_w = marker
                    .lines()
                    .next()
                    .map(|l| l.metrics().advance)
                    .unwrap_or(0.0);
                cx - marker_w * 0.5
            } else {
                *marker_x
            };
            // Marker on the first body line's baseline. Tag as Lbl
            // (label) so accessibility tools know it's the marker.
            let marker_lines = marker.lines().count();
            let marker_id = if tags.enabled {
                Some(surface.start_tagged(ContentTag::Span(SpanTag::empty())))
            } else {
                None
            };
            if let Some(color) = check {
                draw_checkmark(surface, marker, marker_origin_x, y, *color);
            } else {
                emit_layout(
                    surface,
                    marker,
                    marker_text,
                    marker_origin_x,
                    y,
                    font_cache,
                    0..marker_lines,
                    0.0,
                );
            }
            if let Some(id) = marker_id {
                surface.end_tagged();
                let mut g = TagGroup::new(TagKind::Lbl(Tag::<kind::Lbl>::Lbl));
                g.push(id);
                tags.push_leaf(g.into());
            }
            // Body content of the list item — wrap in LBody for PDF/UA.
            tags.enter(TagGroup::new(TagKind::LBody(Tag::<kind::LBody>::LBody)));
            emit_blocks(surface, body, y, font_cache, links, outline, tags, None);
            tags.leave(); // LBody
        }

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
            caption: _,
        } => {
            let total_width: f32 = column_widths.iter().sum::<f32>()
                + border_thickness * (column_widths.len() as f32 + 1.0);
            let total_height: f32 = rows.iter().map(|r| r.height).sum::<f32>()
                + border_thickness * (rows.len() as f32 + 1.0);

            // 1. Row backgrounds: header rows paint the header colour;
            //    body rows paint their own optional stripe fill (zebra).
            let mut row_top = y + *border_thickness;
            for row in rows {
                let paint = if row.is_header {
                    Some(*header_bg)
                } else {
                    row.fill
                };
                if let Some(color) = paint {
                    let mut pb = PathBuilder::new();
                    pb.move_to(*x, row_top);
                    pb.line_to(*x + total_width, row_top);
                    pb.line_to(*x + total_width, row_top + row.height);
                    pb.line_to(*x, row_top + row.height);
                    pb.close();
                    let path = pb.finish().unwrap();
                    surface.set_fill(Some(Fill {
                        paint: color.into(),
                        opacity: NormalizedF32::ONE,
                        rule: Default::default(),
                    }));
                    surface.draw_path(&path);
                }
                // Header column: paint column 0 of body rows with the header
                // colour, on top of any stripe.
                if row.header_column
                    && !row.is_header
                    && let Some(w0) = column_widths.first()
                {
                    let col0_right = *x + *border_thickness * 1.5 + *w0;
                    let mut pb = PathBuilder::new();
                    pb.move_to(*x, row_top);
                    pb.line_to(col0_right, row_top);
                    pb.line_to(col0_right, row_top + row.height);
                    pb.line_to(*x, row_top + row.height);
                    pb.close();
                    let path = pb.finish().unwrap();
                    surface.set_fill(Some(Fill {
                        paint: (*header_bg).into(),
                        opacity: NormalizedF32::ONE,
                        rule: Default::default(),
                    }));
                    surface.draw_path(&path);
                }
                row_top += row.height + border_thickness;
            }

            // 2. Cell content. Cells are vertically padded; their content
            //    starts at row_top + padding. Wrap rows in Table → TR
            //    and cells in TH/TD for PDF/UA structure.
            tags.enter(TagGroup::new(TagKind::Table(Tag::<kind::Table>::Table)));
            let mut row_top = y + *border_thickness;
            for row in rows {
                tags.enter(TagGroup::new(TagKind::TR(Tag::<kind::TR>::TR)));
                for cell in &row.cells {
                    let cell_kind = if row.is_header {
                        TagKind::TH(Tag::<kind::TH>::TH(
                            krilla::tagging::TableHeaderScope::Column,
                        ))
                    } else if row.header_column && cell.col == 0 {
                        TagKind::TH(Tag::<kind::TH>::TH(krilla::tagging::TableHeaderScope::Row))
                    } else {
                        TagKind::TD(Tag::<kind::TD>::TD)
                    };
                    tags.enter(TagGroup::new(cell_kind));
                    emit_blocks(
                        surface,
                        &cell.blocks,
                        row_top + *cell_padding,
                        font_cache,
                        links,
                        outline,
                        tags,
                        None,
                    );
                    tags.leave(); // TH/TD
                }
                tags.leave(); // TR
                row_top += row.height + border_thickness;
            }
            tags.leave(); // Table

            // 3. Borders. The rules drawn depend on the table's border
            //    style: `Grid` draws the outer rect plus horizontal row and
            //    vertical column separators; `Horizontal` keeps only the row
            //    separators (no verticals); `None` draws nothing. Drawn last
            //    so they sit on top of any header background that bleeds.
            if !matches!(border_style, TableBorders::None) {
                // Internal rules use `border_color`; the outer frame (top/
                // bottom, plus grid left/right) uses `edge` when set, else
                // the same border colour. Internals are drawn first so the
                // edge colour wins where they meet at the corners.
                let (edge_paint, edge_w) = (*edge).unwrap_or((*border_color, *border_thickness));
                let border_stroke = Stroke {
                    paint: (*border_color).into(),
                    width: *border_thickness,
                    opacity: NormalizedF32::ONE,
                    ..Default::default()
                };
                let edge_stroke = Stroke {
                    paint: edge_paint.into(),
                    width: edge_w,
                    opacity: NormalizedF32::ONE,
                    ..Default::default()
                };

                // Horizontal rule y positions: top, between each row, bottom.
                let mut h_ys = Vec::with_capacity(rows.len() + 1);
                let mut h_y = y + *border_thickness * 0.5;
                h_ys.push(h_y);
                for row in rows {
                    h_y += row.height + *border_thickness;
                    h_ys.push(h_y);
                }
                let last_h = h_ys.len() - 1;

                // Vertical boundary x positions.
                let mut v_xs = Vec::with_capacity(column_widths.len() + 1);
                let mut v_x = *x + *border_thickness * 0.5;
                v_xs.push(v_x);
                for col_w in column_widths {
                    v_x += *col_w + *border_thickness;
                    v_xs.push(v_x);
                }
                let last_v = v_xs.len() - 1;

                let nrows = rows.len();
                let ncols = column_widths.len();
                let top = y;
                let bottom = y + total_height;
                let left = *x;
                let right = *x + total_width;

                // Occupancy grid: which cell (by its `(start_row, start_col)`)
                // owns each `(row, col)`. A merged cell expands over its span,
                // so two adjacent slots sharing an owner mean a span straddles
                // the rule between them — which is then suppressed.
                let mut owner = vec![vec![(usize::MAX, usize::MAX); ncols.max(1)]; nrows.max(1)];
                for (r, row) in rows.iter().enumerate() {
                    for cell in &row.cells {
                        for dr in 0..cell.rowspan.max(1) {
                            for dc in 0..cell.colspan.max(1) {
                                let (rr, cc) = (r + dr, cell.col + dc);
                                if rr < nrows && cc < ncols {
                                    owner[rr][cc] = (r, cell.col);
                                }
                            }
                        }
                    }
                }
                let merged = |a: (usize, usize), b: (usize, usize)| a == b && a.0 != usize::MAX;

                // Internal horizontals (Grid & Horizontal): per-column
                // segments, skipping where a rowspan straddles the boundary.
                surface.set_stroke(Some(border_stroke.clone()));
                for hb in 1..last_h {
                    for c in 0..ncols {
                        if hb < nrows && merged(owner[hb - 1][c], owner[hb][c]) {
                            continue;
                        }
                        let xl = if c == 0 { left } else { v_xs[c] };
                        let xr = if c + 1 == ncols { right } else { v_xs[c + 1] };
                        line(surface, xl, h_ys[hb], xr, h_ys[hb]);
                    }
                }

                // Internal verticals (Grid only): per-row segments, skipping
                // where a colspan straddles the boundary.
                if matches!(border_style, TableBorders::Grid) {
                    for vb in 1..last_v {
                        for r in 0..nrows {
                            if vb < ncols && merged(owner[r][vb - 1], owner[r][vb]) {
                                continue;
                            }
                            let yt = if r == 0 { top } else { h_ys[r] };
                            let yb = if r + 1 == nrows { bottom } else { h_ys[r + 1] };
                            line(surface, v_xs[vb], yt, v_xs[vb], yb);
                        }
                    }
                }

                // Outer frame (edge colour): top + bottom always; left + right
                // for `Grid`.
                surface.set_stroke(Some(edge_stroke));
                line(surface, left, h_ys[0], right, h_ys[0]);
                line(surface, left, h_ys[last_h], right, h_ys[last_h]);
                if matches!(border_style, TableBorders::Grid) {
                    line(surface, v_xs[0], top, v_xs[0], bottom);
                    line(surface, v_xs[last_v], top, v_xs[last_v], bottom);
                }

                surface.set_stroke(None);
            }
        }

        BlockDraw::Float { image, wrap } => {
            // Draw the floated image first (its `x` was placed on the chosen
            // side at layout time), then stack the wrap blocks from the same
            // top `y`. Each wrap block already carries the x / width it was
            // laid out at (narrow + shifted while beside the image, full
            // column once clear), and `emit_blocks` advances y by the same
            // cumulative heights the layout pass used — so image and wrap
            // overlap exactly as intended.
            emit_block(surface, image, y, 0.0, font_cache, links, outline, tags);
            emit_blocks(surface, wrap, y, font_cache, links, outline, tags, None);
        }

        BlockDraw::FloatRegion { text, floats } => {
            // Draw each floated image at its resolved position, then the one
            // prose slice at the region top. The slice's lines already carry
            // their per-line origin / measure (narrowed around whichever
            // floats they overlap), so a plain text-slice emit places them
            // correctly beside and below the images.
            for fl in floats {
                emit_block(
                    surface,
                    &fl.image,
                    y + fl.y_offset,
                    0.0,
                    font_cache,
                    links,
                    outline,
                    tags,
                );
            }
            emit_text_slice(
                surface,
                text,
                y,
                font_cache,
                links,
                tags,
                TagKind::P(Tag::<kind::P>::P),
            );
        }

        BlockDraw::FormField {
            label,
            field_x,
            field_y,
            field_w,
            field_h,
            border,
            thickness,
            hint,
            hint_y,
        } => {
            if let Some(l) = label {
                emit_text_slice(
                    surface,
                    l,
                    y,
                    font_cache,
                    links,
                    tags,
                    TagKind::P(Tag::<kind::P>::P),
                );
            }
            // Stroke the field box (a plain rectangle outline — pure graphics).
            let bx = *field_x;
            let by = y + *field_y;
            let mut pb = PathBuilder::new();
            pb.move_to(bx, by);
            pb.line_to(bx + *field_w, by);
            pb.line_to(bx + *field_w, by + *field_h);
            pb.line_to(bx, by + *field_h);
            pb.close();
            if let Some(path) = pb.finish() {
                // Clear any leftover fill (e.g. the label's last glyph colour)
                // so the box is a stroked outline, not a filled rectangle.
                surface.set_fill(None);
                surface.set_stroke(Some(Stroke {
                    paint: (*border).into(),
                    width: *thickness,
                    opacity: NormalizedF32::ONE,
                    ..Default::default()
                }));
                surface.draw_path(&path);
                surface.set_stroke(None);
            }
            if let Some(h) = hint {
                emit_text_slice(
                    surface,
                    h,
                    y + *hint_y,
                    font_cache,
                    links,
                    tags,
                    TagKind::P(Tag::<kind::P>::P),
                );
            }
        }
    }
}

/// Emit one laid-out text slice: draw its glyphs, its decorations
/// (underline / strikethrough), and register its links — honouring
/// per-line origins so a `{% float %}` wrap renders the same as an
/// ordinary paragraph. `kind` is the structure-tree group the plain
/// (non-link) segments join under (`P`, `Hn`, `Note`). Shared by the
/// `Text` and `Float` arms of [`emit_block`].
#[allow(clippy::too_many_arguments)]
fn emit_text_slice(
    surface: &mut krilla::surface::Surface<'_>,
    slice: &super::block::TextSlice,
    y: f32,
    font_cache: &mut HashMap<u64, Font>,
    links: &mut Vec<DeferredLink>,
    tags: &mut TagAccumulator,
    kind: TagKind,
) {
    if tags.enabled {
        // Tagged path: split content into per-link segments so each link's
        // text can sit inside its own `Link` tag group alongside the
        // corresponding annotation. Plain segments collect into one
        // `P`/`Hn`/`Note` group; link segments are paired with annotations
        // later in mod.rs.
        let segments = emit_layout_segmented(
            surface,
            &slice.layout,
            &slice.text,
            slice.x,
            y,
            font_cache,
            slice.line_range.clone(),
            slice.skip_y,
            &slice.links,
        );
        draw_decorations(
            surface,
            &slice.layout,
            slice.x,
            y,
            slice.line_range.clone(),
            slice.skip_y,
        );
        let mut group = TagGroup::new(kind);
        for seg in &segments {
            if seg.link_idx_in_block.is_none() {
                group.push(seg.id);
            }
        }
        tags.push_leaf(group.into());

        // One DeferredLink per link, holding all per-line rects and matching
        // segment Identifiers. Multi-line wrapped links collapse into a
        // single annotation with quad_points and a single Link tag group
        // downstream.
        for (lidx, lr) in slice.links.iter().enumerate() {
            let mut line_rects = Vec::new();
            collect_link_rects_per_line(
                &slice.layout,
                slice.x,
                y,
                lr,
                &mut line_rects,
                slice.line_range.clone(),
                slice.skip_y,
            );
            if line_rects.is_empty() {
                continue;
            }
            let mut text_ids = Vec::with_capacity(line_rects.len());
            let mut rects = Vec::with_capacity(line_rects.len());
            for (line_idx, rect) in line_rects {
                if let Some(seg) = segments
                    .iter()
                    .find(|s| s.link_idx_in_block == Some(lidx) && s.line_idx == line_idx)
                {
                    text_ids.push(seg.id);
                }
                rects.push(rect);
            }
            links.push(DeferredLink {
                rects,
                href: lr.href.clone(),
                alt: lr.title.clone(),
                text_segment_ids: text_ids,
            });
        }
    } else {
        emit_layout(
            surface,
            &slice.layout,
            &slice.text,
            slice.x,
            y,
            font_cache,
            slice.line_range.clone(),
            slice.skip_y,
        );
        draw_decorations(
            surface,
            &slice.layout,
            slice.x,
            y,
            slice.line_range.clone(),
            slice.skip_y,
        );
        for lr in slice.links.iter() {
            collect_link_rects(
                &slice.layout,
                slice.x,
                y,
                lr,
                links,
                slice.line_range.clone(),
                slice.skip_y,
            );
        }
    }
}

/// Map a markdown heading level to the appropriate krilla heading tag
/// (`H1`–`H6`). Levels above 6 collapse to 6. PDF/UA wants the
/// heading text as a Title attribute.
fn heading_tag_kind(level: u8, title: &str) -> TagKind {
    let lvl = std::num::NonZeroU16::new(level.clamp(1, 6) as u16).unwrap();
    TagKind::Hn(Tag::<kind::Hn>::Hn(lvl, Some(title.to_string())))
}

fn line(surface: &mut krilla::surface::Surface<'_>, x0: f32, y0: f32, x1: f32, y1: f32) {
    let mut pb = PathBuilder::new();
    pb.move_to(x0, y0);
    pb.line_to(x1, y1);
    let path = pb.finish().unwrap();
    surface.draw_path(&path);
}

/// Fill a circle of radius `r` centred at `(cx, cy)`, approximated by
/// four cubic-Bézier quadrants (control-point factor κ ≈ 0.5523). Used
/// for ordered-list marker badges.
/// Draw a vector checkmark for a `{% list type="checkmark" %}` marker, sized
/// and positioned from the (unrendered) text marker's box at `(x, y_top)`.
fn draw_checkmark(
    surface: &mut krilla::surface::Surface<'_>,
    marker: &Layout<rgb::Color>,
    x: f32,
    y_top: f32,
    color: rgb::Color,
) {
    let h = marker.height().max(1.0);
    let baseline = y_top + h * 0.78;
    let s = h * 0.5;
    let mut pb = PathBuilder::new();
    pb.move_to(x, baseline - s * 0.45);
    pb.line_to(x + s * 0.38, baseline);
    pb.line_to(x + s * 0.95, baseline - s);
    let path = pb.finish().unwrap();
    surface.set_stroke(Some(Stroke {
        paint: color.into(),
        width: (s * 0.16).max(0.8),
        opacity: NormalizedF32::ONE,
        ..Default::default()
    }));
    surface.draw_path(&path);
    surface.set_stroke(None);
}

fn fill_circle(
    surface: &mut krilla::surface::Surface<'_>,
    cx: f32,
    cy: f32,
    r: f32,
    color: rgb::Color,
) {
    const K: f32 = 0.552_284_8;
    let o = K * r;
    let mut pb = PathBuilder::new();
    pb.move_to(cx, cy - r);
    pb.cubic_to(cx + o, cy - r, cx + r, cy - o, cx + r, cy);
    pb.cubic_to(cx + r, cy + o, cx + o, cy + r, cx, cy + r);
    pb.cubic_to(cx - o, cy + r, cx - r, cy + o, cx - r, cy);
    pb.cubic_to(cx - r, cy - o, cx - o, cy - r, cx, cy - r);
    pb.close();
    let path = pb.finish().unwrap();
    surface.set_fill(Some(Fill {
        paint: color.into(),
        opacity: NormalizedF32::ONE,
        rule: Default::default(),
    }));
    surface.draw_path(&path);
}

/// Walk a parley `Layout` to find the bounding rect(s) of bytes
/// `[link.start, link.end)` and append them as one `DeferredLink`
/// holding every line rect. A wrapped link therefore produces a
/// single annotation with quad_points downstream rather than one
/// annotation per line.
fn collect_link_rects(
    layout: &Layout<rgb::Color>,
    origin_x: f32,
    origin_y_top: f32,
    link: &LinkRange,
    out: &mut Vec<DeferredLink>,
    line_range: std::ops::Range<usize>,
    skip_y: f32,
) {
    let mut rects = Vec::new();
    collect_link_rects_per_line(
        layout,
        origin_x,
        origin_y_top,
        link,
        &mut rects,
        line_range,
        skip_y,
    );
    if rects.is_empty() {
        return;
    }
    out.push(DeferredLink {
        rects: rects.into_iter().map(|(_, r)| r).collect(),
        href: link.href.clone(),
        alt: link.title.clone(),
        text_segment_ids: Vec::new(),
    });
}

/// Variant of [`collect_link_rects`] that returns each rect together
/// with the parley line index it belongs to so the caller can pair the
/// rect with the matching tagged-text segment.
fn collect_link_rects_per_line(
    layout: &Layout<rgb::Color>,
    origin_x: f32,
    origin_y_top: f32,
    link: &LinkRange,
    out: &mut Vec<(usize, LineRect)>,
    line_range: std::ops::Range<usize>,
    skip_y: f32,
) {
    for (i, line_obj) in layout.lines().enumerate() {
        if i < line_range.start {
            continue;
        }
        if i >= line_range.end {
            break;
        }
        let metrics = line_obj.metrics();
        let baseline = origin_y_top - skip_y + metrics.baseline;
        let line_top = baseline - metrics.ascent;
        let line_height = metrics.ascent + metrics.descent;

        // Include the alignment offset and the per-line origin
        // (`inline_min_coord`, set by `{% float %}`) so link rects track
        // shifted lines. Both are 0 for ordinary left-aligned paragraphs.
        let mut x = origin_x + metrics.offset + metrics.inline_min_coord;
        let mut link_min_x: Option<f32> = None;
        let mut link_max_x: f32 = origin_x;

        for run in line_obj.runs() {
            for cluster in run.visual_clusters() {
                if cluster.is_ligature_continuation() {
                    continue;
                }
                let range = cluster.text_range();
                let in_link = range.start < link.end && range.end > link.start;
                let advance = cluster.advance();
                if in_link {
                    if link_min_x.is_none() {
                        link_min_x = Some(x);
                    }
                    link_max_x = x + advance;
                }
                x += advance;
            }
        }

        if let Some(min_x) = link_min_x {
            out.push((i, (min_x, line_top, link_max_x - min_x, line_height)));
        }
    }
}

// Suppress unused-import warning until we expand.
#[allow(dead_code)]
fn _silence_rgb(_: rgb::Color) {}
#[allow(dead_code)]
fn _silence_point(_: Point) {}

/// Stroke a horizontal line `[x0, x1]` at `y`. Shared by the
/// decoration pass; clears the surface stroke afterwards.
fn stroke_hline(
    surface: &mut krilla::surface::Surface<'_>,
    x0: f32,
    x1: f32,
    y: f32,
    thickness: f32,
    color: rgb::Color,
) {
    if x1 <= x0 {
        return;
    }
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

/// Extend, flush, or open the running span for one decoration kind as
/// [`draw_decorations`] walks clusters. `active` is `Some((y, thickness))`
/// when the current cluster carries the decoration; `None` flushes any
/// open span.
fn step_decoration(
    surface: &mut krilla::surface::Surface<'_>,
    open: &mut Option<(f32, f32, f32, f32, rgb::Color)>,
    active: Option<(f32, f32)>,
    color: rgb::Color,
    x: f32,
    advance: f32,
) {
    match active {
        // Same vertical position → extend the current span.
        Some((y, _)) if matches!(open, Some(s) if (s.2 - y).abs() < 0.05) => {
            open.as_mut().unwrap().1 = x + advance;
        }
        // Newly active, or the position changed (e.g. a differently-sized
        // run) → flush any open span and start a fresh one.
        Some((y, thickness)) => {
            if let Some((x0, x1, sy, t, c)) = open.take() {
                stroke_hline(surface, x0, x1, sy, t, c);
            }
            *open = Some((x, x + advance, y, thickness, color));
        }
        // Not decorated → flush.
        None => {
            if let Some((x0, x1, sy, t, c)) = open.take() {
                stroke_hline(surface, x0, x1, sy, t, c);
            }
        }
    }
}

/// Paint text decorations — underline and strikethrough — in one pass over
/// the layout. parley records the decoration on a run's resolved style
/// (`Style::underline` / `Style::strikethrough`) but this krilla bridge
/// draws only glyphs, so the rules are stroked here. Vertical position and
/// default thickness come from the run's font metrics (`RunMetrics`); an
/// explicit `UnderlineSize` (which links carry) overrides the thickness.
/// The colour follows the text brush — link text is already tinted, so its
/// underline matches. Each contiguous decorated span becomes one stroke.
fn draw_decorations(
    surface: &mut krilla::surface::Surface<'_>,
    layout: &Layout<rgb::Color>,
    origin_x: f32,
    origin_y_top: f32,
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
        let baseline = origin_y_top - skip_y + line.metrics().baseline;
        let mut x = origin_x + line.metrics().offset + line.metrics().inline_min_coord;
        // One running span per decoration kind: (x0, x1, y, thickness, colour).
        let mut underline: Option<(f32, f32, f32, f32, rgb::Color)> = None;
        let mut strike: Option<(f32, f32, f32, f32, rgb::Color)> = None;
        for run in line.runs() {
            let rm = run.metrics();
            for cluster in run.visual_clusters() {
                if cluster.is_ligature_continuation() {
                    if let Some(s) = underline.as_mut() {
                        s.1 = x;
                    }
                    if let Some(s) = strike.as_mut() {
                        s.1 = x;
                    }
                    continue;
                }
                let advance = cluster.advance();
                let Some(glyph) = cluster.glyphs().next() else {
                    // No glyph (unexpected) → treat as a gap: flush open spans.
                    if let Some((x0, x1, y, t, c)) = underline.take() {
                        stroke_hline(surface, x0, x1, y, t, c);
                    }
                    if let Some((x0, x1, y, t, c)) = strike.take() {
                        stroke_hline(surface, x0, x1, y, t, c);
                    }
                    x += advance;
                    continue;
                };
                let style = &layout.styles()[glyph.style_index as usize];
                let ul = style.underline.as_ref().map(|d| {
                    (
                        baseline - d.offset.unwrap_or(rm.underline_offset),
                        d.size.unwrap_or(rm.underline_size).max(0.4),
                    )
                });
                let st = style.strikethrough.as_ref().map(|d| {
                    (
                        baseline - d.offset.unwrap_or(rm.strikethrough_offset),
                        d.size.unwrap_or(rm.strikethrough_size).max(0.4),
                    )
                });
                step_decoration(surface, &mut underline, ul, style.brush, x, advance);
                step_decoration(surface, &mut strike, st, style.brush, x, advance);
                x += advance;
            }
        }
        if let Some((x0, x1, y, t, c)) = underline.take() {
            stroke_hline(surface, x0, x1, y, t, c);
        }
        if let Some((x0, x1, y, t, c)) = strike.take() {
            stroke_hline(surface, x0, x1, y, t, c);
        }
    }
}
