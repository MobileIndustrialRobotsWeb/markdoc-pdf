# markdoc-pdf

Render an Adeptus [Markdoc](https://github.com/EntSP/markdoc) source
file to a PDF. Both a **CLI** for tech writers iterating locally and
a **library** that [Scriptor](https://github.com/EntSP/scriptor) will
shell out to in the production pipeline.

```
.mdoc source  ──►  markdoc::parse  ──►  expand partials
                                        resolve crossrefs
                                        evaluate conditionals
                                        transform
                                                │
                                                ▼
                                      ┌────────────────────┐
                                      │   markdoc-pdf      │
                                      │  ├ paginate        │
                                      │  ├ shape (parley)  │
                                      │  ├ hyphenate       │
                                      │  ├ syntax-highlight│
                                      │  └ emit (krilla)   │
                                      └─────────┬──────────┘
                                                ▼
                                               PDF
```

Frontmatter (`title`, `authors`, `language`, `releaseDate`, …)
drives PDF `/Info` metadata and the `{title}` / `{date}` / `{authors}`
template variables in headers and footers. The typed view comes from
[`flux-types`](https://github.com/EntSP/flux-types), which `markdoc-pdf` re-exports as
`markdoc_pdf::flux`.

## CLI

```sh
markdoc-pdf \
    --input  manual.mdoc \
    --output manual.pdf \
    --style  examples/themes/technical-manual.style.toml
```

`--style` is optional; omitting it gives a clean A4 default (Noto
Sans, no decoration). `--assets-root` defaults to the input file's
parent directory, which fits the typical "one folder per document"
layout — see [`GETTING_STARTED.md`](GETTING_STARTED.md).

Errors are written as a single `error: …` line on stderr followed
by a `hint: …` follow-up that suggests the most likely fix. The
process exits non-zero so build pipelines fail loudly.

## Library

The same pipeline is exposed as a library for callers that already
have a parsed `RenderableTreeNode`:

```rust
use markdoc_pdf::render::{render_pdf_with, RenderContext, Style};
use markdoc_pdf::assets::FsAssetResolver;

let style    = Style::default();
let resolver = FsAssetResolver::new("./assets");
let ctx      = RenderContext { title: "Manual".into(), ..Default::default() };

let pdf: Vec<u8> = render_pdf_with(&rendered, &style, &resolver, &ctx);
```

Public modules:

| Module | Purpose |
|--------|---------|
| `render` | The PDF emit pipeline (`render_pdf_with`, `Style`, `RenderContext`) |
| `assets` | `AssetResolver` trait + `FsAssetResolver` |
| `dates` | ISO-8601 ↔ PDF date helpers used by the metadata layer |
| `flux` | Re-export of [`flux-types`](https://github.com/EntSP/flux-types) for callers that want the typed frontmatter |

## What it handles

- **Markdoc primitives**: headings, paragraphs, lists, blockquotes,
  fenced code (syntax-highlighted via the style's theme), tables,
  inline emphasis, links, images (`![](…)` and `{% media %}` / `{% img %}`,
  referenced by path or by Arca asset `id`).
- **Tags from the Flux spec**: `{% callout %}`, `{% caption %}`,
  `{% footnote %}`, `{% tag id="…" /%}`, `{% tagref id="…" /%}`,
  `{% partial file="…" /%}` (recursive, cycle-detected),
  `{% if … %}` / `{% else %}` chains.
- **Layout & form constructs** (renderer extensions beyond the spec):
  side-by-side `{% columns %}`, text-wrapping `{% float %}` (a single image,
  or several inline-anchored "magazine" floats), print form fields
  (`{% input %}`, pure graphics so PDF/A-safe), inline colour
  (`{% color %}` / `{% c %}`), and custom list markers (`{% list %}`).
- **Frontmatter as metadata**: `/Title`, `/Author`, `/Creator`,
  `/CreationDate` written into the PDF `/Info` dictionary; same
  fields surface as `{title}`, `{authors}`, `{date}` template
  variables in the style's header/footer/cover sections.
- **Layout features**: hyphenation (embedded en-US dictionary,
  swappable per language), per-page footnote pools, automatic
  Figure N / Table N numbering, table of contents, list of figures,
  list of tables, cover page, watermarks, and even-page padding
  (`pad_to_even`) for duplex (double-sided) printing.
- **Cross-document references**: see [`CROSS_DOC.md`](CROSS_DOC.md).
  Intra-document refs become PDF `GoTo` annotations; cross-doc
  refs are expected to be rewritten upstream by Adeptus before they
  reach this crate (unresolved ones render as visible
  `[doc#anchor]` placeholders so they aren't silently dropped).

For the current state against the in-flight Flux spec PR, see
[`FLUX_COMPATIBILITY.md`](FLUX_COMPATIBILITY.md).

## Themes

Ready-made styles live in [`examples/themes/`](examples/themes/):

| Theme | For |
|-------|-----|
| `technical-manual.style.toml` | Product manuals |
| `academic-paper.style.toml` | Papers, theses |
| `book.style.toml` | Long-form books |
| `whitepaper.style.toml` | Marketing / sales whitepapers |
| `letter.style.toml` | Letters, memos |
| `release-notes.style.toml` | Changelogs, release notes |
| `draft-report.style.toml` | Internal review drafts (watermarked) |
| `cheatsheet.style.toml` | One-page reference cards |
| `poster-a0` … `poster-a3` | Conference posters |

The style file controls page geometry, fonts, colours,
headers/footers, the cover page, the watermark, hyphenation, syntax
highlighting and font loading. See
[`examples/themes/README.md`](examples/themes/README.md) for the
full knob inventory.

## Installation

See [`INSTALL.md`](INSTALL.md) — covers `cargo install`, manual
binary copy, Windows (PowerShell + WSL), container builds, and CI
pre-built artefacts.

Short version on Linux/macOS, assuming `markdoc/`, `flux-types/` and
`markdoc-pdf/` are checked out side-by-side:

```sh
cd markdoc-pdf
cargo install --path . --locked
markdoc-pdf --version
```

## Why a separate crate

`markdoc` parses; `flux-types` types the frontmatter; **this crate**
emits PDF. The split keeps:

- `markdoc` reusable from anywhere (web renderer, validators,
  CLIs that never touch PDF).
- The PDF stack (krilla, parley, usvg, fontdb, hyphenation,
  image) out of every consumer that doesn't need it — and it's a
  big stack.
- Output formats addable without forking the parser. A future
  `markdoc-epub` slots in alongside.

In the eventual production pipeline, [Scriptor](https://github.com/EntSP/scriptor)
invokes the `markdoc-pdf` binary as a subprocess, then optionally
hands the result to [`pdf-stamp`](../pdf-stamp) for per-recipient
personalisation. The two crates are intentionally separate processes
so the canonical render can be cached and stamped on demand without
re-rendering.

## Dependencies

Notable crates this pulls in:

| Crate | Why |
|-------|-----|
| [`krilla`](https://crates.io/crates/krilla) + `krilla-svg` | PDF emit; SVG conversion |
| [`parley`](https://crates.io/crates/parley) | Text shaping and line breaking |
| [`usvg`](https://crates.io/crates/usvg) | SVG parsing |
| [`fontdb`](https://crates.io/crates/fontdb) | Font discovery |
| [`image`](https://crates.io/crates/image) | Raster image decode |
| [`hyphenation`](https://crates.io/crates/hyphenation) | Embedded en-US patterns |
| `clap`, `toml`, `serde`, `time`, `ureq`, `thiserror` | CLI / config / IO plumbing |

## Status

Tracked against the in-flight Flux spec PR in
[`FLUX_COMPATIBILITY.md`](FLUX_COMPATIBILITY.md). MVP-complete for
the **local tech-writer loop** — edit `.mdoc`, run the binary,
open the PDF, iterate. Adeptus / Scriptor integration (cross-doc
rewrite, per-render Context, asset URI resolution beyond the
filesystem) is post-MVP.

## License

MIT.

## MiR Modifications

* Support for light indicator table and image grids
* Support for various image sizes
* Implemetned following page-break rules: 
  * Page-breaks cannot occur right after a heading
  * Page-breaks cannot occur just before a list starts (Page-breaks can be in the middle of a list)
  * Page-breaks cannot occur in the middle of a list item, unless the list item is longer than a page.
  * There must be at least three lines of text, a complete paragraph, an image, a notice block, or a table before a page-break.
* Support for column size control
* Bold and italics fix
* Condition and varaible support.