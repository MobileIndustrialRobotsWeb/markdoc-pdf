# Theme catalogue

Ready-made `.style.toml` files for common document types. Pass any of
them to `markdoc-pdf` with `--style themes/<name>.style.toml`. Mix and
match: every field has a sensible default, so a theme can override
just the parts that matter and inherit the rest.

| File | Use for | Notable features |
|------|---------|------------------|
| `technical-manual.style.toml` | Product manuals, reference docs | A4, ToC + LoF + LoT, chapter/section header, page-of-total footer, hyphenation |
| `academic-paper.style.toml` | Papers, theses | US Letter, looser leading, justified body, footnote pool, page-number footer |
| `draft-report.style.toml` | Internal review reports | PDF/UA-1 export, "DRAFT" diagonal watermark, build-date footer |
| `letter.style.toml` | Letters, memos | Generous margins, no decoration, single-page friendly |
| `book.style.toml` | Long-form publications | 6×9", verso/recto headers (chapter on recto, title on verso), small body font, ToC |
| `release-notes.style.toml` | Changelogs, release notes | Compact dense layout, version in header, no front matter |
| `whitepaper.style.toml` | Sales / position papers | Large title page from frontmatter, ToC at start, generous margins, justified body, hyphenation |
| `cheatsheet.style.toml` | Quick references, CLI cards | Landscape A4, narrow margins, dense body; ideal for one-page reference cards |
| `poster-a3.style.toml` | A3 wall posters (297 × 420 mm) | Single page, body text sized for ~1.5 m reading distance |
| `poster-a2.style.toml` | A2 conference-board posters (420 × 594 mm) | Single page, body text sized for 2-3 m reading distance |
| `poster-a1.style.toml` | A1 conference / poster-session (594 × 841 mm) | Single page, body text sized for 3-4 m reading distance |
| `poster-a0.style.toml` | A0 lecture-hall / large display (841 × 1189 mm) | Single page, body text sized for 4-6 m reading distance |

## Authoring your own

Themes are just TOML — start from one of the above as a baseline and
override what you want. Every field has a default, so you set only the
parts that matter and inherit the rest. The complete knob inventory is the
[Style reference](#style-reference) below; the source of truth is
`markdoc-pdf/src/render/style.rs`.

## Cover pages

Source `.mdoc` documents stay output-agnostic — they don't include a
title-page tag. Instead, enable a synthesised cover page in the style:

```toml
[coverpage]
enabled = true
top_margin = 180
title_font_size = 36
subtitle = "{description}"   # template — `{title}`, `{description}`, `{date}`

[coverpage.logo]
src = "logo.svg"             # any AssetResolver URI
width = 200
height = 80

[page_decoration]
skip_first_page = true       # cover page renders without header/footer
```

The renderer pulls the title, description, authors, and date from the
document's frontmatter (via `RenderContext`), draws the logo if
present, then forces a page break so body content starts on page 2.

---

# Style reference

Every field of `Style` is a top-level TOML key. The whole struct (and every
nested one) is `#[serde(default)]`, so **all keys are optional** — a partial
`.style.toml` overrides only what it names and inherits the rest. Nested
structs become `[table]` / `[table.sub]` sections.

**Types.** *number* values are `f64` in PDF points unless noted (`em` for
line-heights, `deg` for rotation, a `0.0–1.0` fraction for opacities). A
**colour** deserializes from either an inline array `[r, g, b]` or a table
`{ r = …, g = …, b = … }`, each channel `0–255`; defaults below are shown as
the RGB triple. An *optional* field absent from the TOML stays unset (and
usually inherits another value, noted in the description). *Enum* values are
listed inline in the Type column.

**Runtime-filled values.** Cover-page title/description/authors/date and the
`{…}` template variables below come from the document frontmatter at render
time — the style controls the *template strings and layout*, which is
everything listed here. `schema_version` (default `1`) is a reserved stamp
with no rendering effect.

### Template variables

These substitute into header / footer / banner / cover-page template
strings at render time:

`{page}`, `{total}`, `{title}`, `{chapter}` (most recent h1), `{section}`,
`{date}`, and `{copyright_years}` — plus any frontmatter field by name (e.g.
`{version}`, `{language}`). A template whose variables all resolve to empty
is skipped; an unknown `{name}` is left literal so typos are visible.

**`{copyright_years}`** is a derived value for legal footers: a single year
(`2026`) when the document's `releaseDate` year equals the current year
(or is absent), or an en-dash span (`2024–2026`) when the release year is
earlier. It uses the *current* year at render time, so — unlike a hardcoded
year — it updates on each re-render.

```toml
[page_decoration.footer]
left = "Copyright © {copyright_years} by ACME LLC."
```

renders `Copyright © 2026 by ACME LLC.` for a same-year release, or
`Copyright © 2024–2026 by ACME LLC.` once the current year moves past the
`releaseDate`.

### Page geometry

| Key | Type | Default | Description |
|---|---|---|---|
| `page_width` | number (pt) | `595.0` | Page width in PDF points (A4). |
| `page_height` | number (pt) | `842.0` | Page height in PDF points (A4). |
| `margin_x` | number (pt) | `72.0` | Left/right page margin. |
| `margin_y` | number (pt) | `72.0` | Top/bottom page margin. |
| `pad_to_even` | bool | `false` | If the doc ends on an odd page, append one blank (still header/footer/watermark-bearing) page so the physical total is even, for duplex printing. Counts toward `{total}`. |

### Fonts

| Key | Type | Default | Description |
|---|---|---|---|
| `font_paths` | array of string | `[]` | Extra `.ttf`/`.otf` font files to load before layout; families they expose become referenceable. Loaded once. |
| `body_font_families` | array of string | `[]` | Body-text family names in fallback order. Empty = bundled Noto Sans + multi-script fallbacks. |
| `code_font_family` | string | `"Noto Sans Mono"` | Monospace family for code. |

### Body text & paragraphs

| Key | Type | Default | Description |
|---|---|---|---|
| `body_font_size` | number (pt) | `11.0` | Body text size. |
| `body_line_height` | number (em) | `1.5` | Body line-height multiplier. |
| `paragraph_space_after` | number (pt) | `8.0` | Vertical gap after each paragraph. |
| `text_color` | colour | `20,20,20` | Body text colour. |
| `text_align` | `"left"` \| `"justify"` \| `"center"` \| `"right"` | `"left"` | Alignment of body prose (paragraphs, list-item & callout bodies). Headings/captions/table cells unaffected. |

### Links

`[link]` — a visual cue only; the PDF link annotation is always created.

| Key | Type | Default | Description |
|---|---|---|---|
| `link.color` | colour | `20,95,175` | Colour applied to link glyphs. |
| `link.italic` | bool | `false` | Render link text italic. |
| `link.bold` | bool | `false` | Render link text bold (weight 700). |
| `link.underline` | bool | `false` | Draw a stroked underline below each link line. |
| `link.underline_thickness` | number (pt) | `0.6` | Underline stroke thickness; ignored when `underline = false`. |

### Headings

`[heading.h1]` … `[heading.h6]` — each level has the same fields; `color`
unset means the heading inherits body `text_color`.

Fields: `font_size` (pt), `font_weight` (number, e.g. 700), `space_before`
(pt), `space_after` (pt), `color` (optional colour). Per-level defaults:

| Level | `font_size` | `font_weight` | `space_before` | `space_after` | `color` |
|---|---|---|---|---|---|
| `h1` | `26.0` | `700.0` | `18.0` | `12.0` | unset |
| `h2` | `21.0` | `700.0` | `16.0` | `10.0` | unset |
| `h3` | `17.0` | `700.0` | `14.0` | `8.0` | unset |
| `h4` | `14.0` | `700.0` | `12.0` | `6.0` | unset |
| `h5` | `12.0` | `700.0` | `10.0` | `6.0` | unset |
| `h6` | `11.0` | `700.0` | `10.0` | `4.0` | unset |

`[heading_numbering]` — automatic `1` / `1.1` / `1.1.1` section numbers.

| Key | Type | Default | Description |
|---|---|---|---|
| `heading_numbering.enabled` | bool | `false` | Master switch for auto section numbering. |
| `heading_numbering.max_depth` | number (1–6) | `3` | Deepest level numbered; deeper headings render plain. |
| `heading_numbering.separator` | string | `" "` | String between the number and heading text (e.g. `". "` → `"1.2. Title"`). |

### Lists & markers

| Key | Type | Default | Description |
|---|---|---|---|
| `list_indent` | number (pt) | `24.0` | Indent per list nesting level. |
| `list_item_space_after` | number (pt) | `4.0` | Vertical gap after each list item. |
| `list_marker_gap` | number (pt) | `8.0` | Gap between marker and item text. |

`[list_marker]` — ordered-list numbering & optional badge; bullets
unaffected.

| Key | Type | Default | Description |
|---|---|---|---|
| `list_marker.ordered_sequences` | array of `"decimal"` \| `"lower-alpha"` \| `"upper-alpha"` \| `"lower-roman"` \| `"upper-roman"` | `[]` | Numbering style per nesting depth (index 0 = outermost; cycles). Empty = decimal everywhere. |
| `list_marker.badge` | bool | `false` | Draw each ordered marker centred in a filled circle (trailing `.` dropped). |
| `list_marker.badge_fill` | colour | `223,227,232` | Badge fill colour. |
| `list_marker.badge_text_color` | optional colour | unset (→ body text) | Marker text colour inside a badge. |
| `list_marker.badge_scale` | number | `1.7` | Badge diameter as a multiple of marker font size. |

### Block quotes

| Key | Type | Default | Description |
|---|---|---|---|
| `blockquote_indent` | number (pt) | `24.0` | Left indent of block quotes. |
| `blockquote_bar_width` | number (pt) | `3.0` | Width of the left accent bar. |
| `blockquote_bar_color` | colour | `200,205,215` | Colour of the left bar. |
| `blockquote_text_color` | colour | `80,90,100` | Block-quote text colour. |

### Code blocks & syntax highlighting

| Key | Type | Default | Description |
|---|---|---|---|
| `code_font_size` | number (pt) | `10.0` | Code font size. |
| `code_padding` | number (pt) | `12.0` | Inner padding of code blocks. |
| `code_background` | colour | `245,246,248` | Code-block background fill. |
| `code_text_color` | colour | `40,50,70` | Default code text colour (and fallback for unrecognised languages). |

`[code_highlight]` — per-token-class palette for recognised `language`
fences.

| Key | Type | Default | Description |
|---|---|---|---|
| `code_highlight.keyword` | colour | `170,50,130` | Keyword token colour. |
| `code_highlight.string` | colour | `70,120,50` | String-literal token colour. |
| `code_highlight.comment` | colour | `120,130,140` | Comment token colour. |
| `code_highlight.number` | colour | `190,110,30` | Numeric-literal token colour. |

### Callouts

Global callout knobs:

| Key | Type | Default | Description |
|---|---|---|---|
| `callout_padding` | number (pt) | `12.0` | Inner padding of callout boxes. |
| `callout_accent_width` | number (pt) | `4.0` | Width of the left accent stripe (box decoration). |
| `callout_space_after` | number (pt) | `12.0` | Vertical gap after a callout. |
| `callout_label_size` | number (pt) | `11.0` | Font size for a callout's bold label line. |
| `callout_icon_size` | number (pt) | `20.0` | Square draw size for a callout icon. |
| `callout_icon_gap` | number (pt) | `10.0` | Horizontal gap between icon and label/body column. |
| `callout_rule_thickness` | number (pt) | `0.7` | Stroke thickness for `decoration = "rules"` callouts. |

`[callout_styles.<kind>]` where `<kind>` ∈ `note`, `info`, `warning`,
`caution`, `danger`, `success`, `notice`. Each kind's fields:

| Sub-key | Type | Description |
|---|---|---|
| `background` | colour | Box fill colour. |
| `border` | colour | Box border colour. |
| `accent` | colour | Left accent-bar colour (box) / rule colour (rules). |
| `decoration` | `"box"` \| `"rules"` | Framing — filled box (default) or horizontal rules above/below. |
| `label` | string | Optional bold heading line (e.g. `"WARNING"`); empty = none. |
| `label_color` | optional colour | Label colour; unset = body text colour. |
| `label_centered` | bool (default `false`) | Centre the label across the content column. |
| `icon` | string | Optional icon asset URI/path at top-left; empty = none. |

Per-kind colour defaults (all other fields inherit `decoration = "box"`,
empty label/icon, `label_centered = false`):

| Kind | `background` | `border` | `accent` |
|---|---|---|---|
| `note` | `247,248,250` | `220,225,230` | `120,130,145` |
| `info` | `232,244,253` | `180,213,240` | `54,130,200` |
| `warning` | `255,247,230` | `252,211,166` | `217,119,6` |
| `caution` | `255,247,230` | `252,211,166` | `217,119,6` |
| `danger` | `254,232,232` | `248,187,187` | `204,51,51` |
| `success` | `232,250,240` | `168,220,188` | `46,160,100` |
| `notice` | `245,240,255` | `214,198,240` | `120,80,200` |

### Horizontal rules (`---`)

| Key | Type | Default | Description |
|---|---|---|---|
| `rule_color` | colour | `200,205,215` | Horizontal-rule colour. |
| `rule_thickness` | number (pt) | `0.75` | Horizontal-rule stroke thickness. |
| `rule_space_around` | number (pt) | `12.0` | Vertical space above and below a rule. |

### Captions & figures

| Key | Type | Default | Description |
|---|---|---|---|
| `caption_position` | `"above"` \| `"below"` | `"above"` | Where a caption sits relative to its figure/table. |
| `caption_color` | optional colour | unset (→ `blockquote_text_color`) | Caption text colour; per-caption `{% caption color %}` overrides. |
| `figure_caption_prefix` | string | `"Figure"` | Prefix for figure caption labels (`"Figure N"`). |
| `table_caption_prefix` | string | `"Table"` | Prefix for table caption labels. |
| `caption_separator` | string | `":"` | Separator between the numbered prefix and caption text. |
| `image_title_fallback` | bool | `false` | Use an image's `title` attribute as caption / accessibility text when no `{% caption %}` or `alt` is present. |

### Table of contents / List of figures / List of tables

`[toc]`:

| Key | Type | Default | Description |
|---|---|---|---|
| `toc.enabled` | bool | `false` | Generate a table of contents. |
| `toc.position` | `"start"` \| `"end"` | `"start"` | Where TOC pages are inserted. |
| `toc.title` | string | `"Table of Contents"` | Heading atop the TOC. |
| `toc.title_font_size` | number (pt) | `24.0` | TOC title font size. |
| `toc.entry_font_size` | number (pt) | `11.0` | Font size for TOC entries. |
| `toc.entry_space_after` | number (pt) | `4.0` | Vertical gap after each entry. |
| `toc.entry_indent_per_level` | number (pt) | `16.0` | Indent per heading-level depth. |
| `toc.max_depth` | number (1–6) | `3` | Deepest heading level included. |

`[lof]` (List of Figures) and `[lot]` (List of Tables) — both flat lists
(no per-level indent); `title` optional (defaults to "List of Figures" /
"List of Tables"):

| Key (per section) | Type | Default | Description |
|---|---|---|---|
| `lof.enabled` / `lot.enabled` | bool | `false` | Generate the section. |
| `lof.position` / `lot.position` | `"start"` \| `"end"` | `"start"` | Where it's inserted. |
| `lof.title` / `lot.title` | optional string | unset (→ section name) | Section title. |
| `lof.title_font_size` / `lot.title_font_size` | number (pt) | `24.0` | Title font size. |
| `lof.entry_font_size` / `lot.entry_font_size` | number (pt) | `11.0` | Entry font size. |
| `lof.entry_space_after` / `lot.entry_space_after` | number (pt) | `4.0` | Gap after each entry. |

### Tables

| Key | Type | Default | Description |
|---|---|---|---|
| `table_column_sizing` | `"auto"` \| `"equal"` | `"auto"` | Content-proportional (`auto`) or `available / num_cols` (`equal`). |
| `table_column_weights` | array of number | `[]` | Explicit relative column widths (weights). When the count matches a table, overrides `table_column_sizing` for it. |
| `table_borders` | `"grid"` \| `"horizontal"` \| `"none"` | `"grid"` | Which rules the table draws. |
| `table_cell_padding` | number (pt) | `6.0` | Inner cell padding. |
| `table_border_color` | colour | `210,215,225` | Internal rule colour. |
| `table_border_thickness` | number (pt) | `0.5` | Internal rule thickness. |
| `table_edge_color` | optional colour | unset (→ `table_border_color`) | Outer-frame rule colour (booktabs look). |
| `table_edge_thickness` | optional number (pt) | unset (→ `table_border_thickness`) | Outer-frame rule thickness. |
| `table_header_background` | colour | `240,242,246` | Header-row/column fill. |
| `table_header_text_color` | colour | `20,30,50` | Header text colour. |
| `table_stripe_color` | optional colour | unset (disabled) | Zebra-stripe fill for alternating body rows; per-table `{% table stripe %}` overrides. |
| `table_header_column` | bool | `false` | Treat the first column as row headers (header style + fill + a11y tagging). |
| `table_space_after` | number (pt) | `12.0` | Vertical gap after a table. |

### Footnotes

`[footnote]` — per-page footnote pool.

| Key | Type | Default | Description |
|---|---|---|---|
| `footnote.font_size` | number (pt) | `9.0` | Entry font size. |
| `footnote.line_height` | number (em) | `1.35` | Line-height for entries. |
| `footnote.entry_space_after` | number (pt) | `3.0` | Gap between entries. |
| `footnote.gap_above` | number (pt) | `12.0` | Gap between body content and the separator rule. |
| `footnote.gap_below_rule` | number (pt) | `6.0` | Gap between the rule and the first entry. |
| `footnote.rule_width_frac` | number (0.0–1.0) | `0.3` | Separator-rule width as a fraction of the column. |
| `footnote.rule_thickness` | number (pt) | `0.5` | Separator-rule thickness. |
| `footnote.rule_color` | colour | `150,155,165` | Separator-rule colour. |
| `footnote.text_color` | colour | `70,80,95` | Footnote text colour. |

### Hyphenation

`[hyphenation]`:

| Key | Type | Default | Description |
|---|---|---|---|
| `hyphenation.enabled` | bool | `false` | Insert soft hyphens at Knuth–Liang break points. |
| `hyphenation.language` | string | `"en-us"` | Language tag; only `"en-us"` is bundled. |
| `hyphenation.min_word_chars` | number | `5` | Don't hyphenate words shorter than this. |
| `hyphenation.dictionary_path` | optional string | unset | Path to a `.bincode` pattern file; required for non-bundled languages. |

### Headers & footers

`[page_decoration]`:

| Key | Type | Default | Description |
|---|---|---|---|
| `page_decoration.skip_first_page` | bool | `false` | Render page 1 without header/footer (for a cover/title page). |
| `page_decoration.header` | optional table | unset | See below. |
| `page_decoration.footer` | optional table | unset | Same fields as header. |
| `page_decoration.banner` | optional table | unset | Rich top masthead — see [Notice banner](#notice-banner). |
| `page_decoration.last_page_qr` | optional table | unset | QR stamp in the last content page's bottom-right corner — see [Last-page QR](#last-page-qr). |

`[page_decoration.header]` / `[page_decoration.footer]` (present only if you
define the table):

| Key | Type | Default | Description |
|---|---|---|---|
| `…header.left` | string (template) | `""` | Left-slot text. |
| `…header.center` | string (template) | `""` | Center-slot text. |
| `…header.right` | string (template) | `""` | Right-slot text. |
| `…header.font_size` | number (pt) | `9.0` | Slot text font size. |
| `…header.color` | colour | `110,120,130` | Slot text colour. |
| `…header.margin_from_edge` | number (pt) | `36.0` | Distance from the page edge (top for header, bottom for footer). |
| `…header.rule` | bool | `false` | Draw a separator rule between decoration and body. |
| `…header.rule_color` | colour | `220,225,230` | Rule colour. |
| `…header.rule_thickness` | number (pt) | `0.5` | Rule thickness. |
| `…header.rule_gap` | number (pt) | `4.0` | Gap between text and rule (when `rule = true`). |
| `…header.max_lines` | number | `1` | Max lines a slot may wrap into; pagination reserves space for this many. |
| `…header.logo_left` / `logo_center` / `logo_right` | optional table (logo) | unset | A logo replacing that slot's text — see [Logo spec](#logo-spec). |
| `…header.even` | optional table | unset | Even/verso-page slot overrides (`left`/`center`/`right`, each `""` = inherit). |
| `…header.per_chapter` | table (map: h1-text → slots) | `{}` | Per-h1-chapter slot overrides, keyed by exact h1 text. |

#### Last-page QR

`[page_decoration.last_page_qr]` stamps a small QR code in the bottom-right
corner of the last page that carries content (never a blank duplex-padding
page), above a human-readable caption. `value` / `label` are templates (default
`"{documentNumber}"`) resolved against frontmatter and the usual
`{page}`/`{title}`/… tokens; if `value` leaves an unresolved `{token}` (the
field isn't set) the stamp is skipped, and an empty resolved `label` hides the
caption. Set `margin_bottom` larger than the footer band so the stamp clears
it.

| Key | Type | Default | Description |
|---|---|---|---|
| `…last_page_qr.value` | string (template) | `"{documentNumber}"` | Data encoded in the code. |
| `…last_page_qr.label` | string (template) | `"{documentNumber}"` | Caption below the code; empty hides it. |
| `…last_page_qr.size` | number (pt) | `54.0` | Side length of the code. |
| `…last_page_qr.ecl` | string | `"medium"` | Error correction: `low` / `medium` / `quartile` / `high`. |
| `…last_page_qr.margin_right` | number (pt) | `40.0` | Gap from the page's right edge. |
| `…last_page_qr.margin_bottom` | number (pt) | `48.0` | Gap from the page's bottom edge. |
| `…last_page_qr.color` | colour | `0,0,0` | Module colour (the field stays white). |
| `…last_page_qr.label_font_size` | number (pt) | `8.0` | Caption font size. |

#### Logo spec

Used by `logo_left`/`logo_center`/`logo_right`, `[coverpage.logo]`,
`[coverpage.hero]`, and the banner `logo`/`icon`:

| Key | Type | Default | Description |
|---|---|---|---|
| `…id` | string (template) | `""` | Asset-library GUID (file stem), same as `{% media id="…" /%}`. Tried before `src`. |
| `…src` | string (template) | `""` | Asset URI (`file://`, relative path, `https://`, `arca://`). A bare GUID with no extension is treated as an `id`. |
| `…width` | number (pt) | `0.0` | Display width (no auto-scaling). |
| `…height` | number (pt) | `0.0` | Display height. |
| `…gap` | number (pt) | `6.0` | Gap between logo and slot text (LEFT/RIGHT header/footer slots only). |

### Notice banner

`[page_decoration.banner]` — a tall top masthead. All text fields are
templates; empty fields are skipped. `logo` / `icon` are optional logo
tables (see [Logo spec](#logo-spec)).

| Key | Type | Default | Description |
|---|---|---|---|
| `…banner.height` | number (pt) | `100.0` | Reserved vertical space from the page top. |
| `…banner.logo` | optional table (logo) | unset | Left masthead logo. |
| `…banner.logo_subtitle` | string | `""` | Small line under the logo (e.g. company name). |
| `…banner.logo_subtitle_color` | colour | `140,145,150` | Logo-subtitle colour. |
| `…banner.logo_subtitle_font_size` | number (pt) | `6.0` | Logo-subtitle font size. |
| `…banner.disclaimer` | string | `""` | Wrapping disclaimer paragraph below the logo. |
| `…banner.disclaimer_color` | colour | `150,155,160` | Disclaimer colour. |
| `…banner.disclaimer_font_size` | number (pt) | `8.0` | Disclaimer font size. |
| `…banner.disclaimer_max_lines` | number | `3` | Max lines the disclaimer may wrap into. |
| `…banner.icon` | optional table (logo) | unset | Right-side icon (e.g. warning triangle). |
| `…banner.label` | string | `""` | Label centred under the icon, above the closing rule. |
| `…banner.label_color` | colour | `20,20,20` | Label colour. |
| `…banner.label_font_size` | number (pt) | `11.0` | Label font size. |
| `…banner.note` | string (template) | `""` | Note near the band's bottom (e.g. `"Original language: {language}"`). |
| `…banner.note_color` | colour | `60,60,60` | Note colour. |
| `…banner.note_font_size` | number (pt) | `8.5` | Note font size. |
| `…banner.rule` | bool | `true` | Draw a full-width rule closing the band. |
| `…banner.rule_color` | colour | `180,185,190` | Closing-rule colour. |
| `…banner.rule_thickness` | number (pt) | `0.6` | Closing-rule thickness. |

### Cover page

`[coverpage]` — the TOML key is `coverpage` (one word). Title/description/
authors/date come from frontmatter; templates support `{title}`,
`{description}`, `{date}`, and any frontmatter var. `logo` / `hero` are
optional logo tables (see [Logo spec](#logo-spec)).

| Key | Type | Default | Description |
|---|---|---|---|
| `coverpage.enabled` | bool | `false` | Render a synthesised cover page before body content. |
| `coverpage.logo` | optional table (logo) | unset | Cover logo image. |
| `coverpage.logo_position` | `"above"` \| `"below_title"` | `"above"` | Logo position relative to the title. |
| `coverpage.hero` | optional table (logo) | unset | Optional hero image below the cover metadata. |
| `coverpage.hero_gap` | number (pt) | `40.0` | Gap above the hero image. |
| `coverpage.top_margin` | number (pt) | `200.0` | Vertical space above the first element. |
| `coverpage.logo_to_title_gap` | number (pt) | `32.0` | Gap between logo and title. |
| `coverpage.title_font_size` | number (pt) | `32.0` | Title font size. |
| `coverpage.title_accent` | string (template) | `""` | Inline accent run appended to the title; empty omits it. |
| `coverpage.title_accent_color` | optional colour | unset (→ `text_color`) | Colour for `title_accent`. |
| `coverpage.title_to_subtitle_gap` | number (pt) | `12.0` | Gap between title and subtitle. |
| `coverpage.subtitle` | string (template) | `""` | Subtitle template; empty omits it. |
| `coverpage.subtitle_font_size` | number (pt) | `14.0` | Subtitle font size. |
| `coverpage.subtitle_to_authors_gap` | number (pt) | `24.0` | Gap between subtitle and authors. |
| `coverpage.show_authors` | bool | `true` | Show the authors line. |
| `coverpage.authors_font_size` | number (pt) | `12.0` | Authors font size. |
| `coverpage.authors_to_date_gap` | number (pt) | `8.0` | Gap between authors and date. |
| `coverpage.show_date` | bool | `true` | Show the date line. |
| `coverpage.date_font_size` | number (pt) | `11.0` | Date font size. |
| `coverpage.text_color` | colour | `20,20,20` | Cover text colour. |
| `coverpage.align` | `"center"` \| `"left"` | `"center"` | Alignment of all cover text blocks. |
| `coverpage.detail_lines` | array of string (templates) | `[]` | Extra metadata lines under the title (`"Version: {version}"` …); empty-substitution lines skipped. |
| `coverpage.detail_font_size` | number (pt) | `11.0` | Font size for detail lines. |
| `coverpage.title_to_detail_gap` | number (pt) | `14.0` | Gap below the title before detail lines. |
| `coverpage.detail_line_gap` | number (pt) | `3.0` | Gap between consecutive detail lines. |
| `coverpage.detail_color` | optional colour | unset (→ `text_color`) | Colour for detail lines (typically muted grey). |
| `coverpage.blank_page_after` | bool | `false` | Insert a blank page after the cover so body starts on page 3 (recto). |

### Watermark

`[watermark]` (optional) — absent means no watermark. Tagged as an
`Artifact` so screen readers ignore it.

| Key | Type | Default | Description |
|---|---|---|---|
| `watermark.opacity` | number (0.0–1.0) | `0.15` | Overlay opacity. |
| `watermark.skip_first_page` | bool | `false` | Omit the watermark on page 1 (keep a cover clean). |
| `watermark.kind` | tagged table | text `"DRAFT"` | Image vs text — chosen via a `type` tag (below). |

`[watermark.kind]` is tagged: set `type = "text"` or `type = "image"`. A
`[watermark]` with no `kind` defaults to a text watermark.

*Text* (`type = "text"`):

| Key | Type | Default | Description |
|---|---|---|---|
| `watermark.kind.text` | string | `"DRAFT"` | Overlay text. |
| `watermark.kind.font_size` | number (pt) | `96.0` | Text size. |
| `watermark.kind.color` | colour | `180,180,180` | Text colour. |
| `watermark.kind.rotation_deg` | number (deg) | `-30.0` | Rotation, anti-clockwise (`-30` slants bottom-left → top-right). |

*Image* (`type = "image"`):

| Key | Type | Default | Description |
|---|---|---|---|
| `watermark.kind.src` | string | `""` | Image asset URI/path. |
| `watermark.kind.x` | number (pt) | `0.0` | Top-left origin X. |
| `watermark.kind.y` | number (pt) | `0.0` | Top-left origin Y. |
| `watermark.kind.width` | number (pt) | `0.0` | Stretched width. |
| `watermark.kind.height` | number (pt) | `0.0` | Stretched height. |

### PDF export

| Key | Type | Default | Description |
|---|---|---|---|
| `pdf_export` | `"none"` \| `"a1_b"` \| `"a2_b"` \| `"a3_b"` \| `"a4"` \| `"u_a1"` | `"none"` | Export/validator profile: plain, PDF/A-1b…A-4 archival tiers, or PDF/UA-1 accessibility (tagged PDF). |

### Reserved

| Key | Type | Default | Description |
|---|---|---|---|
| `schema_version` | number | `1` | Version stamp; reserved for future incompatible bumps — no rendering effect. |
