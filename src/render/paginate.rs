//! Greedy bin-packing of `Block`s into pages, with text-block splitting
//! at line boundaries when a paragraph doesn't fit on the current page.
//!
//! Algorithm:
//!   - Maintain a current page accumulator and a worklist of remaining
//!     blocks.
//!   - For each block: if it fits, push it. Otherwise call
//!     `Block::try_split(remaining)`:
//!       * `Whole`  → fits unchanged (accumulator was wrong; place it).
//!       * `Split(head, tail)` → push head, flush page, prepend tail
//!         to the worklist.
//!       * `NoFit`  → block can't be split or zero lines fit. Flush
//!         current page (if any) and place block on a fresh page.
//!         If even on a fresh page it doesn't fit, place anyway —
//!         overflow is tolerated.
//!
//! Before every flush, trailing blocks that would leave an awkward page
//! ending are pulled back onto the worklist:
//!   - no page break immediately after a heading
//!   - no page break just before a list starts
//!   - a heading must be followed by ≥3 lines of text, or by a list /
//!     image / table / callout / notice / other boxed text block, before
//!     a break is allowed
//!
//! When `num_columns > 1`, body blocks flow down the first column, then
//! the second, and so on. Blocks with `column_span` span the full page
//! width and advance every column's cursor to the same y.

use std::collections::VecDeque;

use super::block::{Block, SplitOutcome};

/// Minimum lines of text that must follow a heading on the same page
/// (unless a list item / figure / table / callout follows instead).
const HEADING_MIN_FOLLOW_LINES: usize = 3;

/// Move `current[from..]` back onto the front of `work`, preserving order.
fn retract_from(current: &mut Vec<Block>, from: usize, work: &mut VecDeque<Block>) {
    let drained: Vec<Block> = current.drain(from..).collect();
    for b in drained.into_iter().rev() {
        work.push_front(b);
    }
}

/// Retract a heading and its preceding spacer (if any).
fn retract_heading_at(current: &mut Vec<Block>, heading_idx: usize, work: &mut VecDeque<Block>) {
    let from = if heading_idx > 0 && current[heading_idx - 1].is_spacer() {
        heading_idx - 1
    } else {
        heading_idx
    };
    retract_from(current, from, work);
}

/// Index of the last non-spacer block, if any.
fn last_content_index(current: &[Block]) -> Option<usize> {
    current.iter().rposition(|b| !b.is_spacer())
}

/// True when `after` (blocks following a heading on the same page) is
/// enough to allow a page break: a list item, figure, table, callout /
/// notice / other boxed text block, or at least
/// [`HEADING_MIN_FOLLOW_LINES`] lines of text (a complete multi-line
/// paragraph).
fn heading_has_enough_followers(after: &[Block]) -> bool {
    let after: Vec<&Block> = after.iter().filter(|b| !b.is_spacer()).collect();
    if after.is_empty() {
        return false;
    }
    if after.iter().any(|b| b.is_substantial_follower()) {
        return true;
    }
    let lines: usize = after.iter().map(|b| b.text_line_count()).sum();
    lines >= HEADING_MIN_FOLLOW_LINES
}

/// Pull trailing blocks that would create an awkward page ending back
/// onto `work` before the current page is flushed.
///
/// Rules enforced:
/// 1. Page must not end on a heading.
/// 2. Page must not end just before a list starts (next work item is a
///    list item while the page ends on non-list content).
/// 3. A heading near the page end must be followed by enough content
///    (≥3 text lines, or a list / figure / table / callout / notice /
///    other boxed text block).
///
/// Retracts that would empty the page are skipped — otherwise the same
/// keep-group is placed and pulled back forever when it fits alone but
/// not with the following block (e.g. on a reduced first-page budget).
fn retract_orphan_trailing(
    current: &mut Vec<Block>,
    work: &mut VecDeque<Block>,
    page_budget: f32,
) {
    loop {
        let Some(i) = last_content_index(current) else {
            return;
        };

        let next_h = work
            .front()
            .map(|b| b.height + b.space_after)
            .unwrap_or(0.0);

        // Rule 1 — never end a page on a heading.
        if current[i].is_heading() {
            let from = if i > 0 && current[i - 1].is_spacer() {
                i - 1
            } else {
                i
            };
            if from == 0 {
                break; // would empty the page
            }
            let group_h: f32 = current[from..].iter().map(|b| b.height + b.space_after).sum();
            if group_h + next_h > page_budget {
                break;
            }
            retract_heading_at(current, i, work);
            continue;
        }

        // Rule 2 — never break just before a list starts.
        if work.front().is_some_and(Block::is_list_item) && !current[i].is_list_item() {
            if let Some(h) = current[..i].iter().rposition(|b| b.is_heading()) {
                if !heading_has_enough_followers(&current[h + 1..i + 1]) {
                    let from = if h > 0 && current[h - 1].is_spacer() {
                        h - 1
                    } else {
                        h
                    };
                    if from == 0 {
                        break;
                    }
                    let group_h: f32 =
                        current[from..=i].iter().map(|b| b.height + b.space_after).sum();
                    if group_h + next_h > page_budget {
                        break;
                    }
                    retract_heading_at(current, h, work);
                    continue;
                }
            }
            if i == 0 {
                break;
            }
            let group_h = current[i].height + current[i].space_after;
            if group_h + next_h > page_budget {
                break;
            }
            retract_from(current, i, work);
            continue;
        }

        // Keep-with-next — heading without enough followers.
        if let Some(h) = current.iter().rposition(|b| b.is_heading()) {
            if !heading_has_enough_followers(&current[h + 1..]) {
                let from = if h > 0 && current[h - 1].is_spacer() {
                    h - 1
                } else {
                    h
                };
                if from == 0 {
                    break;
                }
                let group_h: f32 = current[from..].iter().map(|b| b.height + b.space_after).sum();
                if group_h + next_h > page_budget {
                    break;
                }
                retract_heading_at(current, h, work);
                continue;
            }
        }

        break;
    }
}

/// Recompute body height + footnote pool after a retract changed `current`.
fn refresh_after_retract(
    current: &[Block],
    pool_for: &mut dyn FnMut(&[u32]) -> (f32, Vec<Block>),
    body_height: &mut f32,
    pool_height: &mut f32,
    pool_blocks: &mut Vec<Block>,
    numbers: &mut Vec<u32>,
) {
    *body_height = current.iter().map(|b| b.height + b.space_after).sum();
    numbers.clear();
    for b in current {
        b.collect_footnote_numbers(numbers);
    }
    let (h, blocks) = if numbers.is_empty() {
        (0.0, Vec::new())
    } else {
        pool_for(numbers)
    };
    *pool_height = h;
    *pool_blocks = blocks;
}

/// Footnote-aware pagination. The `footnote_pool` callback gets the
/// list of footnote numbers attached to the blocks currently parked on
/// the candidate page; it returns the height that the resulting pool
/// would consume *and* the actual pool blocks, in case the caller
/// later wants to render them. We re-evaluate the pool on every block
/// addition so growing pool height steals from the body budget; if a
/// block plus its footnote bodies no longer fit, it gets bumped to the
/// next page (or split, for splittable text blocks).
///
/// `pool_for` returns `(height, pool_blocks)` for the supplied
/// footnote numbers, or `(0.0, Vec::new())` when there are none.
pub fn paginate_with_footnotes(
    blocks: Vec<Block>,
    page_budget: f32,
    first_page_budget: f32,
    num_columns: u8,
    pool_for: impl FnMut(&[u32]) -> (f32, Vec<Block>),
) -> Vec<(Vec<Block>, Vec<Block>)> {
    if num_columns <= 1 {
        return paginate_with_footnotes_single(blocks, page_budget, first_page_budget, pool_for);
    }
    paginate_with_footnotes_multi(
        blocks,
        page_budget,
        first_page_budget,
        num_columns,
        pool_for,
    )
}

fn paginate_with_footnotes_single(
    blocks: Vec<Block>,
    page_budget: f32,
    first_page_budget: f32,
    mut pool_for: impl FnMut(&[u32]) -> (f32, Vec<Block>),
) -> Vec<(Vec<Block>, Vec<Block>)> {
    let mut pages: Vec<(Vec<Block>, Vec<Block>)> = Vec::new();
    let mut current: Vec<Block> = Vec::new();
    let mut current_body_height = 0.0_f32;
    let mut current_pool_height = 0.0_f32;
    let mut current_pool_blocks: Vec<Block> = Vec::new();
    let mut current_numbers: Vec<u32> = Vec::new();

    let mut work: VecDeque<Block> = blocks.into();

    let flush = |pages: &mut Vec<(Vec<Block>, Vec<Block>)>,
                 current: &mut Vec<Block>,
                 pool: &mut Vec<Block>,
                 body_height: &mut f32,
                 pool_height: &mut f32,
                 numbers: &mut Vec<u32>| {
        let body = std::mem::take(current);
        let pool_blocks = std::mem::take(pool);
        pages.push((body, pool_blocks));
        *body_height = 0.0;
        *pool_height = 0.0;
        numbers.clear();
    };

    while let Some(block) = work.pop_front() {
        if matches!(block.draw, super::block::BlockDraw::PageBreak) {
            if !current.is_empty() {
                retract_orphan_trailing(&mut current, &mut work, page_budget);
                refresh_after_retract(
                    &current,
                    &mut pool_for,
                    &mut current_body_height,
                    &mut current_pool_height,
                    &mut current_pool_blocks,
                    &mut current_numbers,
                );
                if !current.is_empty() {
                    flush(
                        &mut pages,
                        &mut current,
                        &mut current_pool_blocks,
                        &mut current_body_height,
                        &mut current_pool_height,
                        &mut current_numbers,
                    );
                }
            }
            continue;
        }
        let needed = block.height + block.space_after;

        let budget = if pages.is_empty() {
            first_page_budget
        } else {
            page_budget
        };

        let mut new_calls = Vec::new();
        block.collect_footnote_numbers(&mut new_calls);
        let (speculative_pool_h, speculative_pool) = if new_calls.is_empty() {
            (current_pool_height, current_pool_blocks.clone())
        } else {
            let mut combined = current_numbers.clone();
            combined.extend_from_slice(&new_calls);
            pool_for(&combined)
        };

        if current_body_height + needed + speculative_pool_h <= budget {
            current_body_height += needed;
            current_pool_height = speculative_pool_h;
            current_pool_blocks = speculative_pool;
            current_numbers.extend(new_calls);
            current.push(block);
            continue;
        }

        let remaining = budget - current_body_height - speculative_pool_h;
        match block.try_split(remaining.max(0.0)) {
            SplitOutcome::Whole(b) => {
                current_body_height += b.height + b.space_after;
                current_pool_height = speculative_pool_h;
                current_pool_blocks = speculative_pool;
                current_numbers.extend(new_calls);
                current.push(b);
            }
            SplitOutcome::Split(head, mut tail) => {
                let mut head_calls = Vec::new();
                head.collect_footnote_numbers(&mut head_calls);
                let mut combined = current_numbers.clone();
                combined.extend_from_slice(&head_calls);
                let (pool_h, pool_blocks) = pool_for(&combined);
                current_pool_height = pool_h;
                current_pool_blocks = pool_blocks;
                current_numbers = combined;
                current.push(head);
                tail.height = match &tail.draw {
                    super::block::BlockDraw::Text(s) => s.height(),
                    _ => tail.height,
                };
                work.push_front(tail);
                retract_orphan_trailing(&mut current, &mut work, page_budget);
                refresh_after_retract(
                    &current,
                    &mut pool_for,
                    &mut current_body_height,
                    &mut current_pool_height,
                    &mut current_pool_blocks,
                    &mut current_numbers,
                );
                if !current.is_empty() {
                    flush(
                        &mut pages,
                        &mut current,
                        &mut current_pool_blocks,
                        &mut current_body_height,
                        &mut current_pool_height,
                        &mut current_numbers,
                    );
                }
            }
            SplitOutcome::NoFit(b) => {
                if !current.is_empty() {
                    work.push_front(b);
                    retract_orphan_trailing(&mut current, &mut work, page_budget);
                    refresh_after_retract(
                        &current,
                        &mut pool_for,
                        &mut current_body_height,
                        &mut current_pool_height,
                        &mut current_pool_blocks,
                        &mut current_numbers,
                    );
                    if !current.is_empty() {
                        flush(
                            &mut pages,
                            &mut current,
                            &mut current_pool_blocks,
                            &mut current_body_height,
                            &mut current_pool_height,
                            &mut current_numbers,
                        );
                    }
                } else {
                    let h = b.height + b.space_after;
                    let mut block_calls = Vec::new();
                    b.collect_footnote_numbers(&mut block_calls);
                    let (pool_h, pool_blocks) = if block_calls.is_empty() {
                        (0.0, Vec::new())
                    } else {
                        pool_for(&block_calls)
                    };
                    current_pool_height = pool_h;
                    current_pool_blocks = pool_blocks;
                    current_numbers.extend(block_calls);
                    current.push(b);
                    current_body_height = h;
                }
            }
        }
    }
    if !current.is_empty() {
        pages.push((current, current_pool_blocks));
    }
    pages
}

struct MultiColumnState {
    col_heights: Vec<f32>,
    active_col: usize,
}

impl MultiColumnState {
    fn new(num_columns: u8) -> Self {
        let n = num_columns.max(1) as usize;
        Self {
            col_heights: vec![0.0; n],
            active_col: 0,
        }
    }

    fn body_used(&self) -> f32 {
        self.col_heights.iter().copied().fold(0.0_f32, f32::max)
    }

    fn remaining_for(&self, block: &Block, budget: f32, pool_h: f32) -> f32 {
        if block.column_span {
            budget - self.body_used() - pool_h
        } else {
            budget - self.col_heights[self.active_col] - pool_h
        }
    }

    fn place(&mut self, block: &Block, needed: f32) {
        if block.column_span {
            let new_h = self.body_used() + needed;
            for h in &mut self.col_heights {
                *h = new_h;
            }
            self.active_col = 0;
        } else {
            self.col_heights[self.active_col] += needed;
        }
    }

    fn assign_column(&self, block: &mut Block) {
        block.page_column = if block.column_span {
            0
        } else {
            self.active_col as u8
        };
    }

    fn advance_column(&mut self) -> bool {
        if self.active_col + 1 < self.col_heights.len() {
            self.active_col += 1;
            true
        } else {
            false
        }
    }

    fn reset(&mut self) {
        self.col_heights.fill(0.0);
        self.active_col = 0;
    }

    /// Rebuild column cursors from the blocks still on the page after a
    /// retract (each block already carries its `page_column`).
    fn recompute_from(&mut self, current: &[Block]) {
        self.reset();
        for b in current {
            let needed = b.height + b.space_after;
            if b.column_span {
                let new_h = self.body_used() + needed;
                for h in &mut self.col_heights {
                    *h = new_h;
                }
                self.active_col = 0;
            } else {
                let col = (b.page_column as usize).min(self.col_heights.len() - 1);
                self.col_heights[col] += needed;
                self.active_col = col;
            }
        }
    }
}

fn paginate_with_footnotes_multi(
    blocks: Vec<Block>,
    page_budget: f32,
    first_page_budget: f32,
    num_columns: u8,
    mut pool_for: impl FnMut(&[u32]) -> (f32, Vec<Block>),
) -> Vec<(Vec<Block>, Vec<Block>)> {
    let mut pages: Vec<(Vec<Block>, Vec<Block>)> = Vec::new();
    let mut current: Vec<Block> = Vec::new();
    let mut mc = MultiColumnState::new(num_columns);
    let mut current_pool_height = 0.0_f32;
    let mut current_pool_blocks: Vec<Block> = Vec::new();
    let mut current_numbers: Vec<u32> = Vec::new();

    let mut work: VecDeque<Block> = blocks.into();

    let flush = |pages: &mut Vec<(Vec<Block>, Vec<Block>)>,
                 current: &mut Vec<Block>,
                 pool: &mut Vec<Block>,
                 mc: &mut MultiColumnState,
                 pool_height: &mut f32,
                 numbers: &mut Vec<u32>| {
        let body = std::mem::take(current);
        let pool_blocks = std::mem::take(pool);
        pages.push((body, pool_blocks));
        mc.reset();
        *pool_height = 0.0;
        numbers.clear();
    };

    while let Some(mut block) = work.pop_front() {
        if matches!(block.draw, super::block::BlockDraw::PageBreak) {
            if !current.is_empty() {
                retract_orphan_trailing(&mut current, &mut work, page_budget);
                mc.recompute_from(&current);
                let mut unused_body = 0.0;
                refresh_after_retract(
                    &current,
                    &mut pool_for,
                    &mut unused_body,
                    &mut current_pool_height,
                    &mut current_pool_blocks,
                    &mut current_numbers,
                );
                if !current.is_empty() {
                    flush(
                        &mut pages,
                        &mut current,
                        &mut current_pool_blocks,
                        &mut mc,
                        &mut current_pool_height,
                        &mut current_numbers,
                    );
                }
            }
            continue;
        }

        let needed = block.height + block.space_after;
        let budget = if pages.is_empty() {
            first_page_budget
        } else {
            page_budget
        };

        let mut new_calls = Vec::new();
        block.collect_footnote_numbers(&mut new_calls);
        let (speculative_pool_h, speculative_pool) = if new_calls.is_empty() {
            (current_pool_height, current_pool_blocks.clone())
        } else {
            let mut combined = current_numbers.clone();
            combined.extend_from_slice(&new_calls);
            pool_for(&combined)
        };

        if needed <= mc.remaining_for(&block, budget, speculative_pool_h).max(0.0) {
            mc.assign_column(&mut block);
            mc.place(&block, needed);
            current_pool_height = speculative_pool_h;
            current_pool_blocks = speculative_pool;
            current_numbers.extend(new_calls);
            current.push(block);
            continue;
        }

        let remaining = mc.remaining_for(&block, budget, speculative_pool_h);
        match block.try_split(remaining.max(0.0)) {
            SplitOutcome::Whole(b) => {
                let mut b = b;
                mc.assign_column(&mut b);
                mc.place(&b, b.height + b.space_after);
                current_pool_height = speculative_pool_h;
                current_pool_blocks = speculative_pool;
                current_numbers.extend(new_calls);
                current.push(b);
            }
            SplitOutcome::Split(head, mut tail) => {
                let mut head = head;
                mc.assign_column(&mut head);
                mc.place(&head, head.height + head.space_after);

                let mut head_calls = Vec::new();
                head.collect_footnote_numbers(&mut head_calls);
                let mut combined = current_numbers.clone();
                combined.extend_from_slice(&head_calls);
                let (pool_h, pool_blocks) = pool_for(&combined);
                current_pool_height = pool_h;
                current_pool_blocks = pool_blocks;
                current_numbers = combined;
                current.push(head);
                tail.height = match &tail.draw {
                    super::block::BlockDraw::Text(s) => s.height(),
                    _ => tail.height,
                };
                work.push_front(tail);
                retract_orphan_trailing(&mut current, &mut work, page_budget);
                mc.recompute_from(&current);
                let mut dummy_body = 0.0;
                refresh_after_retract(
                    &current,
                    &mut pool_for,
                    &mut dummy_body,
                    &mut current_pool_height,
                    &mut current_pool_blocks,
                    &mut current_numbers,
                );
                if !current.is_empty() {
                    flush(
                        &mut pages,
                        &mut current,
                        &mut current_pool_blocks,
                        &mut mc,
                        &mut current_pool_height,
                        &mut current_numbers,
                    );
                }
            }
            SplitOutcome::NoFit(b) => {
                if !current.is_empty() {
                    work.push_front(b);
                    retract_orphan_trailing(&mut current, &mut work, page_budget);
                    mc.recompute_from(&current);
                    let mut dummy_body = 0.0;
                    refresh_after_retract(
                        &current,
                        &mut pool_for,
                        &mut dummy_body,
                        &mut current_pool_height,
                        &mut current_pool_blocks,
                        &mut current_numbers,
                    );
                    if !current.is_empty() {
                        flush(
                            &mut pages,
                            &mut current,
                            &mut current_pool_blocks,
                            &mut mc,
                            &mut current_pool_height,
                            &mut current_numbers,
                        );
                    }
                } else if mc.advance_column() {
                    work.push_front(b);
                } else {
                    let mut b = b;
                    mc.assign_column(&mut b);
                    mc.place(&b, needed);
                    let mut block_calls = Vec::new();
                    b.collect_footnote_numbers(&mut block_calls);
                    let (pool_h, pool_blocks) = if block_calls.is_empty() {
                        (0.0, Vec::new())
                    } else {
                        pool_for(&block_calls)
                    };
                    current_pool_height = pool_h;
                    current_pool_blocks = pool_blocks;
                    current_numbers.extend(block_calls);
                    current.push(b);
                }
            }
        }
    }
    if !current.is_empty() {
        pages.push((current, current_pool_blocks));
    }
    pages
}

pub fn paginate(blocks: Vec<Block>, page_budget: f32) -> Vec<Vec<Block>> {
    let mut pages: Vec<Vec<Block>> = Vec::new();
    let mut current: Vec<Block> = Vec::new();
    let mut current_height = 0.0_f32;

    let mut work: VecDeque<Block> = blocks.into();

    while let Some(block) = work.pop_front() {
        let needed = block.height + block.space_after;

        if current_height + needed <= page_budget {
            current_height += needed;
            current.push(block);
            continue;
        }

        let remaining = page_budget - current_height;
        match block.try_split(remaining) {
            SplitOutcome::Whole(b) => {
                current_height += b.height + b.space_after;
                current.push(b);
            }
            SplitOutcome::Split(head, tail) => {
                current.push(head);
                work.push_front(tail);
                retract_orphan_trailing(&mut current, &mut work, page_budget);
                current_height = current.iter().map(|b| b.height + b.space_after).sum();
                if !current.is_empty() {
                    pages.push(std::mem::take(&mut current));
                    current_height = 0.0;
                }
            }
            SplitOutcome::NoFit(b) => {
                if !current.is_empty() {
                    work.push_front(b);
                    retract_orphan_trailing(&mut current, &mut work, page_budget);
                    current_height = current.iter().map(|b| b.height + b.space_after).sum();
                    if !current.is_empty() {
                        pages.push(std::mem::take(&mut current));
                        current_height = 0.0;
                    }
                } else {
                    let h = b.height + b.space_after;
                    current.push(b);
                    current_height = h;
                }
            }
        }
    }
    if !current.is_empty() {
        pages.push(current);
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::block::{BlockDraw, OutlineEntry};
    use krilla::color::rgb;

    fn filler(h: f32) -> Block {
        Block {
            height: h,
            space_after: 0.0,
            draw: BlockDraw::Rule {
                x: 0.0,
                width: 10.0,
                thickness: 1.0,
                color: rgb::Color::new(0, 0, 0),
            },
            outline: None,
            anchor_id: None,
            tag_role: None,
            page_column: 0,
            column_span: false,
        }
    }

    fn heading_block(h: f32, space_after: f32) -> Block {
        let mut b = filler(h);
        b.space_after = space_after;
        b.outline = Some(OutlineEntry {
            level: 2,
            text: "Heading".into(),
        });
        b
    }

    fn list_item_block(h: f32, space_after: f32) -> Block {
        use parley::{FontContext, LayoutContext};
        let mut font_cx = FontContext::new();
        let mut layout_cx = LayoutContext::new();
        let builder = layout_cx.ranged_builder(&mut font_cx, "", 1.0, true);
        let mut layout: parley::Layout<rgb::Color> = builder.build("");
        layout.break_all_lines(None);
        Block {
            height: h,
            space_after,
            draw: BlockDraw::ListItem {
                marker: layout,
                marker_text: "•".into(),
                marker_x: 0.0,
                body: Vec::new(),
                ordered: false,
                badge: None,
                check: None,
            },
            outline: None,
            anchor_id: None,
            tag_role: None,
            page_column: 0,
            column_span: false,
        }
    }

    fn callout_block(h: f32, space_after: f32) -> Block {
        Block {
            height: h,
            space_after,
            draw: BlockDraw::BoxedGroup {
                x: 0.0,
                width: 100.0,
                background: None,
                border: None,
                accent_left: None,
                accent_width: 0.0,
                padding: 0.0,
                children: Vec::new(),
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

    #[test]
    fn does_not_break_after_heading_alone() {
        // Page nearly full, then a heading that fits, then a list item that
        // doesn't — heading must move with the list onto the next page.
        let blocks = vec![
            filler(70.0),
            heading_block(20.0, 8.0),
            list_item_block(40.0, 4.0),
        ];
        let pages = paginate(blocks, 100.0);
        assert_eq!(pages.len(), 2);
        assert!(pages[0].iter().all(|b| !b.is_heading() && !b.is_list_item()));
        assert!(pages[1][0].is_heading());
        assert!(pages[1][1].is_list_item());
    }

    #[test]
    fn heading_plus_callout_allows_page_break() {
        // Heading + callout fit; next filler does not. Callout alone is
        // enough follower content, so the break after the callout is ok.
        let blocks = vec![
            filler(40.0),
            heading_block(18.0, 6.0),
            callout_block(30.0, 4.0),
            filler(50.0),
        ];
        // 40+24+34 = 98 fits; +50 does not. Keep-group for heading alone
        // would move heading+callout if callout weren't "enough".
        let pages = paginate(blocks, 100.0);
        assert_eq!(pages.len(), 2);
        assert!(pages[0].iter().any(|b| b.is_heading()));
        assert!(pages[0].iter().any(|b| b.is_boxed_text_block()));
        assert!(pages[1].iter().all(|b| !b.is_heading()));
    }

    #[test]
    fn does_not_break_before_list_start() {
        let blocks = vec![
            filler(70.0),
            filler(20.0), // lead-in stand-in
            list_item_block(40.0, 4.0),
        ];
        // Budget fits filler + lead-in (90), not the list item.
        let pages = paginate(blocks, 100.0);
        assert_eq!(pages.len(), 2, "lead-in must stay with the list");
        assert!(pages[1].iter().any(|b| b.is_list_item()));
        // Lead-in moved with the list — page 0 is only the first filler.
        assert_eq!(pages[0].len(), 1);
        assert_eq!(pages[1].len(), 2);
    }

    #[test]
    fn heading_plus_short_lead_in_moves_with_list() {
        let blocks = vec![
            filler(50.0),
            heading_block(18.0, 6.0),
            filler(12.0), // short lead-in ("The box contains:")
            list_item_block(40.0, 4.0),
        ];
        // Fits filler + heading + lead-in (50+24+12=86), not the list.
        // Keep-group heading+lead-in+list = 18+6+12+40+4 = 80 ≤ 100.
        let pages = paginate(blocks, 100.0);
        assert_eq!(
            pages.len(),
            2,
            "heading + short lead-in must move with the list"
        );
        assert!(pages[1][0].is_heading());
        assert!(pages[1].iter().any(|b| b.is_list_item()));
    }
}
