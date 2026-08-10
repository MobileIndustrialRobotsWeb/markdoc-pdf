//! markdoc-pdf — render a Markdoc source file to PDF.
//!
//! Designed for tech writers iterating locally. Errors are printed
//! plainly to stderr with the file path that caused them and (where
//! possible) a hint about how to fix it.

use clap::Parser;
use flux_types::{FluxFrontmatter, HistoryEntry};
use markdoc::{
    Context, Node, evaluate_conditionals, parse,
    partials::{FsPartialResolver, expand_partials},
    resolve_crossrefs, resolve_footnotes, transform_with_context,
    types::{Config, NodeType, Scalar},
};
use markdoc_pdf::assets::FsAssetResolver;
use markdoc_pdf::dates;
use markdoc_pdf::render::{RenderContext, Style, render_pdf_with};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    name = "markdoc-pdf",
    author,
    version,
    about = "Render a Markdoc source file (.mdoc) to PDF.",
    long_about = "Render a Markdoc source file (.mdoc) to PDF.\n\n\
                  Frontmatter (title / authors / language / releaseDate / …) \
                  drives the PDF metadata and the {title}/{date}/etc. header-footer \
                  template variables. Pass --style to pick from the built-in theme \
                  catalogue or supply your own .style.toml."
)]
struct Args {
    /// Path to the input `.mdoc` file.
    #[arg(short, long, value_name = "FILE")]
    input: PathBuf,

    /// Where to write the output PDF.
    #[arg(short, long, value_name = "FILE")]
    output: PathBuf,

    /// Optional style file (TOML). Examples ship in
    /// `markdoc-pdf/examples/themes/`; passing none uses the built-in
    /// default style (A4, Noto Sans, no decoration).
    #[arg(short, long, value_name = "FILE")]
    style: Option<PathBuf>,

    /// Root directory used to resolve relative `<img>` and
    /// `{% media %}` `src` attributes. Defaults to the input file's
    /// parent directory — fits the typical layout of one folder per
    /// document with media siblings.
    #[arg(long, value_name = "DIR")]
    assets_root: Option<PathBuf>,

    /// Build-time variable, repeatable: `--var key=value`. Exposed to the
    /// document as `$key` in `{% … %}` interpolation, `{% if %}` conditions,
    /// and tag attributes. The value is always treated as a string.
    #[arg(long = "var", value_name = "KEY=VALUE")]
    var: Vec<String>,

    /// JSON file of build-time variables (same shape as Markdoc config).
    /// Top-level keys become `$key` in conditionals and interpolation.
    /// `--var` entries override keys from this file.
    #[arg(long, value_name = "FILE")]
    variables: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            for hint in e.hints() {
                eprintln!("hint:  {hint}");
            }
            ExitCode::from(1)
        }
    }
}

fn run(args: &Args) -> Result<(), AppError> {
    // ── Validate inputs early so we can give path-aware errors. ────
    if !args.input.exists() {
        return Err(AppError::InputMissing(args.input.clone()));
    }
    if let Some(s) = &args.style
        && !s.exists()
    {
        return Err(AppError::StyleMissing(s.clone()));
    }
    if let Some(parent) = args.output.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        return Err(AppError::OutputDirMissing(parent.to_path_buf()));
    }
    let assets_root = args
        .assets_root
        .clone()
        .or_else(|| args.input.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    if !assets_root.exists() {
        return Err(AppError::AssetsRootMissing(assets_root));
    }

    // ── Load style. ────────────────────────────────────────────────
    let style = match args.style.as_ref() {
        Some(path) => Style::from_toml_file(path)
            .map_err(|e| AppError::StyleLoad(path.clone(), e.to_string()))?,
        None => Style::default(),
    };

    // ── Collect build-time variables (`--variables` file + `--var`). ─
    let mut cli_vars: HashMap<String, Scalar> = HashMap::new();
    if let Some(path) = &args.variables {
        if !path.exists() {
            return Err(AppError::VariablesMissing(path.clone()));
        }
        let raw = fs::read_to_string(path).map_err(|e| AppError::Read(path.clone(), e))?;
        let json: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| AppError::VariablesLoad(path.clone(), e.to_string()))?;
        if let serde_json::Value::Object(map) = json {
            for (key, value) in map {
                if key == "$schema" {
                    continue;
                }
                cli_vars.insert(key, json_to_scalar(&value));
            }
        }
    }
    for pair in &args.var {
        match pair.split_once('=') {
            Some((k, v)) => {
                cli_vars.insert(k.trim().to_string(), Scalar::String(v.to_string()));
            }
            None => eprintln!(
                "warning: ignoring --var {pair:?} (expected key=value, e.g. --var version=3.2)"
            ),
        }
    }

    // ── Read + parse the source. ───────────────────────────────────
    let source =
        fs::read_to_string(&args.input).map_err(|e| AppError::Read(args.input.clone(), e))?;
    // Variables resolve at transform time via Context (below), not at parse.
    let doc =
        parse(&source, None).map_err(|e| AppError::Parse(args.input.clone(), e.to_string()))?;

    // Stitch a composed work together: if the root frontmatter carries a
    // Flux `sections` manifest, append a `{% partial %}` per section file so
    // the partial expansion below includes them all as one book.
    let doc = expand_sections(doc, &args.input);

    // Expand `{% partial file="..." /%}` references against the input
    // file's parent directory. Partials' own `{% partial %}` tags are
    // resolved recursively; cycles are detected and reported. Inline
    // `{% $var %}` interpolation in a partial / section resolves later,
    // against the transform Context (which carries the root frontmatter).
    let partial_root = args
        .input
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let partial_resolver = FsPartialResolver::new(partial_root);
    let doc = expand_partials(&doc, &partial_resolver)
        .map_err(|e| AppError::Partials(args.input.clone(), e.to_string()))?;

    // Expand `{% document-history /%}` into a table built from the composed
    // document's `documentHistory` frontmatter — after partial expansion so the
    // tag may live in a partial and still read the root document's history.
    let doc = expand_document_history(doc);

    // Normalise CommonMark footnotes (`[^id]` references + `[^id]:` bodies)
    // into `{% footnote %}` tags — after partial / section expansion so a
    // reference and its definition in different files still resolve.
    let doc = resolve_footnotes(&doc);

    let doc = resolve_crossrefs(&doc);
    // Seed the evaluation context with the document's own frontmatter, so
    // conditionals, tag attributes, and inline `{% $var %}` interpolation can
    // read `$markdoc.frontmatter.*` (e.g. `{% qr
    // value=$markdoc.frontmatter.documentNumber /%}`). Then overlay build-time
    // `--var` values as top-level `$key`. Everything resolves here, at
    // transform time, against this one Context — so a stitched-in section sees
    // the composed book's frontmatter, not its own dropped header.
    let base_ctx = match doc.attributes.get("frontmatter") {
        Some(fm) => Context::new().with_frontmatter(fm.clone()),
        None => Context::new(),
    };
    let ctx = cli_vars
        .iter()
        .fold(base_ctx, |c, (k, v)| c.with_variable(k.clone(), v.clone()));
    let doc = evaluate_conditionals(&doc, &ctx)
        .map_err(|e| AppError::Conditionals(args.input.clone(), e.to_string()))?;
    let rendered = transform_with_context(&doc, &Config::default(), &ctx)
        .map_err(|e| AppError::Transform(args.input.clone(), e.to_string()))?;

    let resolver = FsAssetResolver::new(&assets_root);

    // ── Pull doc metadata from frontmatter. A document with no YAML
    //    block is fine (fields fall back to defaults), but frontmatter
    //    that is present yet unparsable is surfaced as a warning rather
    //    than silently dropped — otherwise a single bad field would
    //    quietly strip the title, dates and everything else.
    let fm_opt = match FluxFrontmatter::from_node(&doc) {
        Ok(fm) => Some(fm),
        Err(flux_types::FluxError::NoFrontmatter) => None,
        Err(e) => {
            eprintln!(
                "warning: ignoring unparsable frontmatter in {} ({e}); \
                 title/date metadata may be missing",
                args.input.display()
            );
            None
        }
    };
    let creation_date = fm_opt
        .as_ref()
        .and_then(|fm| {
            fm.update_date
                .as_deref()
                .or(fm.release_date.as_deref())
        })
        .and_then(dates::parse_iso)
        .or_else(|| Some(dates::now()));
    let authors = fm_opt
        .as_ref()
        .map(|fm| fm.authors.clone())
        .unwrap_or_default();
    let creator = fm_opt
        .as_ref()
        .and_then(|fm| fm.creator.clone())
        .or_else(|| Some("markdoc-pdf".to_string()));
    let date_string = fm_opt.as_ref().and_then(|fm| {
        fm.update_date
            .as_deref()
            .or(fm.release_date.as_deref())
            .map(dates::iso_to_date_only)
    });

    // Expose the document's own frontmatter as template variables for
    // header/footer and cover-page templates (`{version}`, `{language}`,
    // … — whatever the author wrote). The renderer stays domain-agnostic:
    // every scalar frontmatter field is surfaced under its authored key,
    // with no hard-coded field names here. Unset fields simply never
    // appear, so their `{name}` token stays literal (and detail lines
    // that resolve to nothing are skipped).
    let mut vars: HashMap<String, String> = HashMap::new();
    collect_frontmatter_vars(&doc, &mut vars);
    // Pre-compute the copyright year span (a generic derived value, not a
    // frontmatter field): a single year when the first-release year
    // equals — or is missing for — the current year, otherwise
    // `first–current`.
    let first_year = fm_opt
        .as_ref()
        .and_then(|fm| fm.release_date.as_deref())
        .and_then(dates::year_of);
    vars.insert(
        "copyright_years".to_string(),
        dates::copyright_year_span(first_year, dates::current_year()),
    );

    let render_ctx = RenderContext {
        title: fm_opt
            .as_ref()
            .and_then(|fm| fm.title.clone())
            .unwrap_or_default(),
        language: fm_opt.as_ref().and_then(|fm| fm.language.clone()),
        description: fm_opt.as_ref().and_then(|fm| fm.description.clone()),
        authors,
        creator,
        producer: Some(format!("markdoc-pdf {}", env!("CARGO_PKG_VERSION"))),
        creation_date,
        date_string,
        vars,
    };

    let pdf = render_pdf_with(&rendered, &style, &resolver, &render_ctx);

    fs::write(&args.output, &pdf).map_err(|e| AppError::Write(args.output.clone(), e))?;
    eprintln!(
        "wrote {} ({} bytes) from {}",
        args.output.display(),
        pdf.len(),
        args.input.display()
    );
    Ok(())
}

/// Stitch a composed work together from its `sections` manifest. When the
/// root document's frontmatter carries a Flux `sections` list — an ordered
/// tree of `[section, [subsection, …]]` file paths — append a
/// `{% partial %}` for each, in order (section then its subsections), after
/// the root's own content (the copyright page, ToC, …). The partial
/// expansion that follows includes each file, dropping its frontmatter so
/// the root's work-level frontmatter governs the whole book, with the usual
/// recursion + cycle detection. A document with no `sections` (the common
/// case) is returned unchanged.
fn expand_sections(mut doc: Node, input: &Path) -> Node {
    let sections = match FluxFrontmatter::from_node(&doc) {
        Ok(fm) => fm.sections,
        Err(_) => return doc, // no / unparsable frontmatter — nothing to stitch
    };
    if sections.is_empty() {
        return doc;
    }
    let base = input.parent().unwrap_or_else(|| Path::new("."));
    for section in &sections {
        for path in std::iter::once(&section.path).chain(section.subsections.iter()) {
            let file = section_partial_path(path, base);
            let mut attrs = HashMap::new();
            attrs.insert("file".to_string(), Scalar::String(file));
            doc.push(Node::new(
                NodeType::Tag,
                attrs,
                Vec::new(),
                Some("partial".to_string()),
            ));
        }
    }
    doc
}

/// Resolve a `sections` path to a `{% partial %}` `file=` value relative to
/// the manifest's directory: strip a leading `/` (the Flux example paths are
/// document-root-relative, not filesystem-absolute) and, when the path has
/// no extension, probe `.mdoc`, `.md`, then `.markdoc` (the Flux spec's
/// extension), defaulting to `.mdoc` so a genuinely missing file surfaces as
/// the standard partial-not-found error.
fn section_partial_path(raw: &str, base: &Path) -> String {
    let rel = raw.trim().trim_start_matches('/');
    if Path::new(rel).extension().is_some() {
        return rel.to_string();
    }
    for ext in ["mdoc", "md", "markdoc"] {
        let cand = format!("{rel}.{ext}");
        if base.join(&cand).is_file() {
            return cand;
        }
    }
    format!("{rel}.mdoc")
}

/// Internal tag name authored as `{% document-history /%}`.
const DOC_HISTORY_TAG: &str = "document-history";

/// Replace every `{% document-history /%}` with a heading + table built from
/// the composed document's `documentHistory` frontmatter (typed as
/// [`HistoryEntry`]). An empty or absent history drops the tag entirely.
fn expand_document_history(mut doc: Node) -> Node {
    let history = match FluxFrontmatter::from_node(&doc) {
        Ok(fm) => fm.document_history,
        Err(_) => return doc, // no / unparsable frontmatter — nothing to expand
    };
    replace_history_tags(&mut doc, &history);
    doc
}

fn replace_history_tags(node: &mut Node, history: &[HistoryEntry]) {
    let mut i = 0;
    while i < node.children.len() {
        let child = &node.children[i];
        if child.node_type == NodeType::Tag && child.tag.as_deref() == Some(DOC_HISTORY_TAG) {
            let title = match child.attributes.get("title") {
                Some(Scalar::String(s)) => Some(s.clone()),
                _ => None,
            };
            let replacement = document_history_nodes(history, title.as_deref());
            let n = replacement.len();
            node.children.splice(i..=i, replacement);
            i += n; // skip the generated nodes (don't recurse into them)
        } else {
            replace_history_tags(&mut node.children[i], history);
            i += 1;
        }
    }
}

/// Build the heading + table nodes for a document history by generating the
/// equivalent Markdoc source and re-parsing it — the parser produces exactly
/// the node shapes the table renderer expects, and cell text is `|`-escaped.
/// An empty history yields nothing (the tag is simply removed).
fn document_history_nodes(history: &[HistoryEntry], title: Option<&str>) -> Vec<Node> {
    if history.is_empty() {
        return Vec::new();
    }
    let mut src = String::new();
    match title {
        Some("") => {} // caller asked for no heading
        Some(t) => src.push_str(&format!("## {t}\n\n")),
        None => src.push_str("## Document history\n\n"),
    }
    src.push_str("| Version | Date | Description |\n| --- | --- | --- |\n");
    let cell = |v: &Option<String>| {
        v.as_deref()
            .unwrap_or("")
            .replace('|', "\\|")
            .replace('\n', " ")
    };
    for e in history {
        src.push_str(&format!(
            "| {} | {} | {} |\n",
            cell(&e.version),
            cell(&e.date),
            cell(&e.description)
        ));
    }
    parse(&src, None).map(|d| d.children).unwrap_or_default()
}

/// Convert a JSON value from `--variables` into Markdoc's `Scalar` type.
fn json_to_scalar(value: &serde_json::Value) -> Scalar {
    match value {
        serde_json::Value::Null => Scalar::Null,
        serde_json::Value::Bool(b) => Scalar::Boolean(*b),
        serde_json::Value::Number(n) => Scalar::Number(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Scalar::String(s.clone()),
        serde_json::Value::Array(items) => {
            Scalar::Array(items.iter().map(json_to_scalar).collect())
        }
        serde_json::Value::Object(map) => Scalar::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_scalar(v)))
                .collect(),
        ),
    }
}

/// Surface every scalar frontmatter field as a template variable keyed
/// by its authored name. Nested values (arrays / objects) and empty
/// strings are skipped — they have no single sensible string form. The
/// renderer never sees the field *names*, so it stays domain-agnostic:
/// whatever the document declares (`version`, `language`, a custom
/// `productLine`, …) becomes referenceable as `{name}`.
fn collect_frontmatter_vars(doc: &markdoc::ast::Node, vars: &mut HashMap<String, String>) {
    let Some(Scalar::Object(map)) = doc.attributes.get("frontmatter") else {
        return;
    };
    for (key, value) in map {
        if let Some(s) = scalar_to_template_string(value) {
            vars.insert(key.clone(), s);
        }
    }
}

/// Render a scalar as a flat template string. Whole-number floats print
/// without a fractional part (YAML integers arrive as `f64`). Non-scalar
/// or empty values yield `None`.
fn scalar_to_template_string(value: &Scalar) -> Option<String> {
    match value {
        Scalar::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Scalar::String(_) => None,
        Scalar::Number(n) if n.is_finite() && n.fract() == 0.0 => Some(format!("{}", *n as i64)),
        Scalar::Number(n) => Some(n.to_string()),
        Scalar::Boolean(b) => Some(b.to_string()),
        Scalar::Null | Scalar::Array(_) | Scalar::Object(_) => None,
    }
}

/// Writer-facing error type. Each variant carries the file path that
/// caused it (where applicable) and produces a one-line message; the
/// `hints` method returns optional follow-up advice that's printed
/// after the error.
#[derive(Debug)]
enum AppError {
    InputMissing(PathBuf),
    StyleMissing(PathBuf),
    OutputDirMissing(PathBuf),
    AssetsRootMissing(PathBuf),
    StyleLoad(PathBuf, String),
    VariablesMissing(PathBuf),
    VariablesLoad(PathBuf, String),
    Read(PathBuf, std::io::Error),
    Write(PathBuf, std::io::Error),
    Parse(PathBuf, String),
    Partials(PathBuf, String),
    Conditionals(PathBuf, String),
    Transform(PathBuf, String),
}

impl AppError {
    fn hints(&self) -> Vec<String> {
        match self {
            AppError::InputMissing(_) => vec![
                "check the path is right (relative paths are resolved against the current directory).".into(),
            ],
            AppError::StyleMissing(_) => vec![
                "see markdoc-pdf/examples/themes/ for ready-made styles, or omit --style for the built-in default.".into(),
            ],
            AppError::OutputDirMissing(p) => vec![
                format!("create the directory first: mkdir -p {}", p.display()),
            ],
            AppError::AssetsRootMissing(_) => vec![
                "--assets-root must point at an existing directory; defaults to the input file's parent.".into(),
            ],
            AppError::StyleLoad(_, msg) if msg.contains("missing field") => vec![
                "the style file is missing a required field — start from one of the example themes and tweak.".into(),
            ],
            AppError::StyleLoad(_, _) => vec![
                "if you're new to the style format, copy markdoc-pdf/examples/themes/letter.style.toml and edit.".into(),
            ],
            AppError::Parse(_, _) => vec![
                "Markdoc parses CommonMark plus {% tag %} extensions. Common gotchas: unbalanced tags, missing /%} on self-closing tags.".into(),
            ],
            AppError::Partials(_, msg) if msg.contains("cycle") => vec![
                "two or more partials include each other transitively — break the loop or extract the shared content into a third file.".into(),
            ],
            AppError::Partials(_, _) => vec![
                "partial paths are resolved against the input file's directory; check spelling and that the file exists.".into(),
            ],
            AppError::Conditionals(_, _) => vec![
                "{% if expr %} branches must reference variables defined in frontmatter, via --var / --variables, or via Context.".into(),
            ],
            AppError::Transform(_, _) => vec![
                "transform errors usually mean a tag failed schema validation — check the spelling and required attributes.".into(),
            ],
            _ => Vec::new(),
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::InputMissing(p) => write!(f, "input file not found: {}", p.display()),
            AppError::StyleMissing(p) => write!(f, "style file not found: {}", p.display()),
            AppError::OutputDirMissing(p) => {
                write!(f, "output directory does not exist: {}", p.display())
            }
            AppError::AssetsRootMissing(p) => {
                write!(f, "--assets-root does not exist: {}", p.display())
            }
            AppError::StyleLoad(p, msg) => {
                write!(f, "couldn't load style {}: {msg}", p.display())
            }
            AppError::VariablesMissing(p) => {
                write!(f, "variables file not found: {}", p.display())
            }
            AppError::VariablesLoad(p, msg) => {
                write!(f, "couldn't load variables {}: {msg}", p.display())
            }
            AppError::Read(p, e) => write!(f, "couldn't read {}: {e}", p.display()),
            AppError::Write(p, e) => write!(f, "couldn't write {}: {e}", p.display()),
            AppError::Parse(p, e) => write!(f, "couldn't parse {}: {e}", p.display()),
            AppError::Partials(p, e) => {
                write!(f, "partial expansion failed for {}: {e}", p.display())
            }
            AppError::Conditionals(p, e) => {
                write!(f, "conditional evaluation failed in {}: {e}", p.display())
            }
            AppError::Transform(p, e) => {
                write!(f, "transform failed in {}: {e}", p.display())
            }
        }
    }
}

impl std::error::Error for AppError {}

// `Path` import suppression — only used in trait bound.
#[allow(dead_code)]
fn _unused(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_strings_and_bools() {
        assert_eq!(
            scalar_to_template_string(&Scalar::String("6.1".into())).as_deref(),
            Some("6.1")
        );
        // Blank / whitespace-only strings are dropped.
        assert_eq!(
            scalar_to_template_string(&Scalar::String("  ".into())),
            None
        );
        assert_eq!(
            scalar_to_template_string(&Scalar::Boolean(true)).as_deref(),
            Some("true")
        );
    }

    #[test]
    fn whole_number_floats_print_as_integers() {
        // YAML integers arrive as f64 — render `75371702`, not `75371702.0`.
        assert_eq!(
            scalar_to_template_string(&Scalar::Number(75371702.0)).as_deref(),
            Some("75371702")
        );
        assert_eq!(
            scalar_to_template_string(&Scalar::Number(2.5)).as_deref(),
            Some("2.5")
        );
    }

    #[test]
    fn non_scalar_values_are_skipped() {
        assert_eq!(scalar_to_template_string(&Scalar::Null), None);
        assert_eq!(scalar_to_template_string(&Scalar::Array(vec![])), None);
        assert_eq!(
            scalar_to_template_string(&Scalar::Object(std::collections::HashMap::new())),
            None
        );
    }

    #[test]
    fn section_paths_default_to_mdoc_and_keep_explicit_ext() {
        let base = Path::new("/no/such/dir");
        // Extensionless → `.mdoc`; a leading `/` is stripped (doc-relative).
        assert_eq!(section_partial_path("intro", base), "intro.mdoc");
        assert_eq!(
            section_partial_path("/manual/safety/x", base),
            "manual/safety/x.mdoc"
        );
        // An explicit extension is preserved — including Flux's `.markdoc`.
        assert_eq!(section_partial_path("a/b.md", base), "a/b.md");
        assert_eq!(section_partial_path("a/b.markdoc", base), "a/b.markdoc");
    }

    #[test]
    fn section_paths_probe_markdoc_then_prefer_native_mdoc() {
        // A real temp dir so the extension probe can stat files on disk.
        let dir = std::env::temp_dir().join(format!("mdpdf-sections-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("chapters")).unwrap();

        // Only a `.markdoc` file exists → an extensionless entry finds it.
        fs::write(dir.join("chapters/intro.markdoc"), b"# Intro\n").unwrap();
        assert_eq!(
            section_partial_path("chapters/intro", &dir),
            "chapters/intro.markdoc"
        );

        // When both exist, the native `.mdoc` wins (probe order).
        fs::write(dir.join("chapters/intro.mdoc"), b"# Intro\n").unwrap();
        assert_eq!(
            section_partial_path("chapters/intro", &dir),
            "chapters/intro.mdoc"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expand_sections_appends_one_partial_per_section_and_subsection() {
        let src = r#"---
title: Book
sections:
- - "intro"
  - []
- - "safety/overview"
  - - "safety/messages"
    - "safety/precautions"
---

# Copyright
"#;
        let doc = markdoc::parser::parse(src, None).unwrap();
        let before = doc.children.len();
        let out = expand_sections(doc, Path::new("book.mdoc"));
        let partials: Vec<&str> = out
            .children
            .iter()
            .filter(|c| c.tag.as_deref() == Some("partial"))
            .filter_map(|c| match c.attributes.get("file") {
                Some(Scalar::String(s)) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        // section, then its subsections, in order — four files total.
        assert_eq!(
            partials,
            [
                "intro.mdoc",
                "safety/overview.mdoc",
                "safety/messages.mdoc",
                "safety/precautions.mdoc",
            ]
        );
        assert_eq!(out.children.len(), before + 4);
    }

    #[test]
    fn expand_sections_noop_without_manifest() {
        let doc = markdoc::parser::parse("---\ntitle: Plain\n---\n\n# Body\n", None).unwrap();
        let before = doc.children.len();
        let out = expand_sections(doc, Path::new("d.mdoc"));
        assert_eq!(out.children.len(), before);
        assert!(
            out.children
                .iter()
                .all(|c| c.tag.as_deref() != Some("partial"))
        );
    }
}
