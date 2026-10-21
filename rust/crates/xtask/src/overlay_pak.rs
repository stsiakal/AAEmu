//! `cargo xtask overlay-pak`: build a small overlay pak for the Bevy client (and server client-file
//! sources) from later-version `game_pak` archives.
//!
//! The overlay is written as a standard TypeA (`WIBO`) pak with the XLGames key, the same variant
//! the 1.2 `game_pak` uses and what [`aaemu_commons::aapak::AAPak::create`] writes by default.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use aaemu_commons::aapak::{filetime_to_date_time, AAPak, PakFileType};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use roxmltree::Document;
use serde_json::{json, Value};

const TEXTURE_EXTENSIONS: &[&str] = &["mtl", "dds", "tif", "tga", "png", "bmp"];

/// Options of one `overlay-pak` run.
#[derive(Debug, Clone)]
struct Options {
    sources: Vec<PathBuf>,
    list: PathBuf,
    closure: bool,
    closure_anim: bool,
    excludes: Vec<PathBuf>,
    out: PathBuf,
    report: Option<PathBuf>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut sources = Vec::new();
        let mut excludes = Vec::new();
        let mut list = None;
        let mut out = None;
        let mut report = None;
        let mut closure = false;
        let mut closure_anim = false;
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let mut value = |name: &str| -> Result<PathBuf> {
                match it.next() {
                    Some(v) => Ok(PathBuf::from(v)),
                    None => bail!("{name} needs a value"),
                }
            };
            match arg.as_str() {
                "--source" => sources.push(value("--source")?),
                "--exclude-present-in" => excludes.push(value("--exclude-present-in")?),
                "--list" => list = Some(value("--list")?),
                "--out" => out = Some(value("--out")?),
                "--report" => report = Some(value("--report")?),
                "--closure" => closure = true,
                "--closure-anim" => {
                    closure = true;
                    closure_anim = true;
                }
                other => bail!("unknown option `{other}`"),
            }
        }
        let sources = required(sources, "--source")?;
        let list = list.context("--list is required")?;
        let out = out.context("--out is required")?;
        if !list.is_file() {
            bail!("list file not found: {}", list.display());
        }
        for source in &sources {
            require_pak(source)?;
        }
        for exclude in &excludes {
            require_pak(exclude)?;
        }
        Ok(Self {
            sources,
            list,
            closure,
            closure_anim,
            excludes,
            out,
            report,
        })
    }
}

fn required<T>(value: Vec<T>, name: &str) -> Result<Vec<T>> {
    if value.is_empty() {
        bail!("{name} is required (repeatable)");
    }
    Ok(value)
}

fn require_pak(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("pak not found: {}", path.display());
    }
    Ok(())
}

pub fn run(args: &[String]) -> Result<()> {
    let options = Options::parse(args)?;
    let summary = build(&options)?;
    println!(
        "Overlay: {} (TypeA/WIBO), {} files, {} bytes",
        options.out.display(),
        summary.written,
        summary.written_bytes
    );
    println!(
        "Included: {} new, {} changed; skipped {} identical",
        summary.new, summary.changed, summary.skipped_identical
    );
    if !summary.missing.is_empty() {
        println!(
            "Missing/ambiguous references: {} (written to the report)",
            summary.missing.len()
        );
        for item in summary.missing.iter().take(10) {
            println!(
                "  {} -> {} ({})",
                item.referenced_by, item.reference, item.reason
            );
        }
        if summary.missing.len() > 10 {
            println!("  ... {} more", summary.missing.len() - 10);
        }
    }
    if let Some(report) = &options.report {
        if let Some(parent) = report.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(report, serde_json::to_vec_pretty(&summary.report)?)
            .with_context(|| format!("writing report {}", report.display()))?;
        println!("Report: {}", report.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// sources
// ---------------------------------------------------------------------------------------------

struct SourcePak {
    path: PathBuf,
    pak: AAPak,
    /// lower-case pak path -> canonical path
    names: HashMap<String, String>,
    /// lower-case material stem -> `.mtl` paths
    mtl_stems: HashMap<String, Vec<String>>,
}

impl SourcePak {
    fn open(path: &Path) -> Result<Self> {
        let pak = AAPak::open(path, true)
            .with_context(|| format!("opening source pak {}", path.display()))?;
        let mut names = HashMap::new();
        let mut mtl_stems: HashMap<String, Vec<String>> = HashMap::new();
        for file in pak.files() {
            let lower = file.name.to_ascii_lowercase();
            names.entry(lower).or_insert_with(|| file.name.clone());
            if extension(&file.name) == "mtl" {
                if let Some(stem) = file_stem(&file.name) {
                    mtl_stems
                        .entry(stem.to_ascii_lowercase())
                        .or_default()
                        .push(file.name.clone());
                }
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            pak,
            names,
            mtl_stems,
        })
    }
}

struct SourcePaks {
    paks: Vec<SourcePak>,
}

impl SourcePaks {
    fn open(paths: &[PathBuf]) -> Result<Self> {
        let mut paks = Vec::with_capacity(paths.len());
        for path in paths {
            paks.push(SourcePak::open(path)?);
        }
        Ok(Self { paks })
    }

    /// `(source index, canonical name)`: the first source that has the file wins.
    fn resolve(&self, candidates: &[String]) -> Option<(usize, String)> {
        for (index, pak) in self.paks.iter().enumerate() {
            for candidate in candidates {
                for form in path_forms(candidate) {
                    if let Some(name) = pak.names.get(&form.to_ascii_lowercase()) {
                        return Some((index, name.clone()));
                    }
                }
            }
        }
        None
    }

    fn resolve_path(&self, raw: &str) -> Option<(usize, String)> {
        let normalized = normalize_ref(raw)?;
        self.resolve(std::slice::from_ref(&normalized))
    }

    /// Resolves a bare material name (`fe_m_ar_leather162`) to its unique `.mtl` stem.
    fn resolve_mtl_stem(&self, stem: &str) -> StemResolution {
        let key = stem.to_ascii_lowercase();
        for (index, pak) in self.paks.iter().enumerate() {
            if let Some(paths) = pak.mtl_stems.get(&key) {
                if paths.len() == 1 {
                    return StemResolution::Found(index, paths[0].clone());
                }
                return StemResolution::Ambiguous(paths.clone());
            }
        }
        StemResolution::Missing
    }

    fn read(&self, source: usize, name: &str) -> Result<Vec<u8>> {
        self.paks[source]
            .pak
            .read_file(name)?
            .with_context(|| format!("{} missing from {}", name, self.paks[source].path.display()))
    }
}

enum StemResolution {
    Found(usize, String),
    Ambiguous(Vec<String>),
    Missing,
}

// ---------------------------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------------------------

/// Normalises a pak path: `/` separators, no quotes, no `./`, no leading `/`.
fn normalize_ref(raw: &str) -> Option<String> {
    let mut value = raw
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .replace('\\', "/");
    while value.starts_with("./") {
        value.drain(..2);
    }
    while value.contains("//") {
        value = value.replace("//", "/");
    }
    let value = value.trim_start_matches('/').to_string();
    if value.is_empty() || value.contains("..") {
        return None;
    }
    Some(value)
}

/// A pak path and its `game/`-prefixed / unprefixed variants.
fn path_forms(normalized: &str) -> Vec<String> {
    let mut forms = vec![normalized.to_string()];
    if normalized.to_ascii_lowercase().starts_with("game/") {
        forms.push(normalized[5..].to_string());
    } else {
        forms.push(format!("game/{normalized}"));
    }
    forms
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

fn file_stem(name: &str) -> Option<&str> {
    let file = name.rsplit('/').next()?;
    file.rsplit_once('.').map(|(stem, _)| stem).or(Some(file))
}

fn parent_dir(name: &str) -> Option<&str> {
    name.rsplit_once('/').map(|(dir, _)| dir)
}

fn sibling_mtl(name: &str) -> Option<String> {
    let (dir, file) = match name.rsplit_once('/') {
        Some((dir, file)) => (dir, file),
        None => ("", name),
    };
    let stem = file.rsplit_once('.').map(|(stem, _)| stem)?;
    Some(if dir.is_empty() {
        format!("{stem}.mtl")
    } else {
        format!("{dir}/{stem}.mtl")
    })
}

fn has_extension(name: &str, wanted: &[&str]) -> bool {
    let ext = extension(name);
    wanted.contains(&ext.as_str())
}

/// Lines of a `--list` file, with `#` comments, quotes, backslashes and blank lines handled.
fn parse_list(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = if index == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.split(" #").next().unwrap_or(line).trim();
        if let Some(path) = normalize_ref(line) {
            out.push(path);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// closure references
// ---------------------------------------------------------------------------------------------

struct Reference {
    raw: String,
    /// Candidate paths, most specific first (resolved with the `game/` prefix rules).
    candidates: Vec<String>,
}

#[derive(Debug)]
struct MissingRef {
    referenced_by: String,
    reference: String,
    reason: String,
}

struct ResolvedRef {
    source: usize,
    name: String,
}

struct Collected {
    resolved: Vec<ResolvedRef>,
    missing: Vec<MissingRef>,
    warnings: Vec<String>,
}

fn push_resolved(resolved: &mut Vec<ResolvedRef>, source: usize, name: String) {
    if !resolved
        .iter()
        .any(|r| r.source == source && r.name.eq_ignore_ascii_case(&name))
    {
        resolved.push(ResolvedRef { source, name });
    }
}

fn push_missing(
    missing: &mut Vec<MissingRef>,
    referenced_by: &str,
    reference: &str,
    reason: String,
) {
    if !missing
        .iter()
        .any(|m| m.referenced_by == referenced_by && m.reference == reference && m.reason == reason)
    {
        missing.push(MissingRef {
            referenced_by: referenced_by.to_string(),
            reference: reference.to_string(),
            reason,
        });
    }
}

fn resolve_reference(
    sources: &SourcePaks,
    referenced_by: &str,
    reference: &Reference,
    resolved: &mut Vec<ResolvedRef>,
    missing: &mut Vec<MissingRef>,
) {
    match sources.resolve(&reference.candidates) {
        Some((source, name)) => push_resolved(resolved, source, name),
        None => push_missing(
            missing,
            referenced_by,
            &reference.raw,
            "not found in any source".into(),
        ),
    }
}

fn collect_references(
    name: &str,
    data: &[u8],
    sources: &SourcePaks,
    closure_anim: bool,
) -> Collected {
    let mut out = Collected {
        resolved: Vec::new(),
        missing: Vec::new(),
        warnings: Vec::new(),
    };
    let ext = extension(name);
    match ext.as_str() {
        "cdf" => {
            let text = match std::str::from_utf8(data) {
                Ok(text) => text,
                Err(error) => {
                    out.warnings.push(format!("{name}: not UTF-8 ({error})"));
                    return out;
                }
            };
            match xml_attribute_refs(text, &["File", "Material", "Model"]) {
                Ok(refs) => {
                    for raw in refs {
                        add_path_reference(name, &raw, sources, &mut out);
                    }
                }
                Err(error) => out.warnings.push(format!("{name}: {error}")),
            }
        }
        "mtl" => {
            let text = match std::str::from_utf8(data) {
                Ok(text) => text,
                Err(error) => {
                    out.warnings.push(format!("{name}: not UTF-8 ({error})"));
                    return out;
                }
            };
            match xml_attribute_refs(text, &["File"]) {
                Ok(refs) => {
                    for raw in refs {
                        if has_extension(&raw, TEXTURE_EXTENSIONS) {
                            add_path_reference(name, &raw, sources, &mut out);
                        }
                    }
                }
                Err(error) => out.warnings.push(format!("{name}: {error}")),
            }
        }
        "chr" | "cgf" | "skin" => {
            if let Some(sibling) = sibling_mtl(name) {
                if let Some((source, resolved)) = sources.resolve_path(&sibling) {
                    push_resolved(&mut out.resolved, source, resolved);
                }
            }
            let (paths, tokens) = embedded_refs(data);
            for raw in paths {
                add_path_reference(name, &raw, sources, &mut out);
            }
            for token in tokens {
                if !shares_asset_prefix(name, &token) {
                    continue;
                }
                match sources.resolve_mtl_stem(&token) {
                    StemResolution::Found(source, resolved) => {
                        push_resolved(&mut out.resolved, source, resolved)
                    }
                    StemResolution::Ambiguous(paths) => push_missing(
                        &mut out.missing,
                        name,
                        &token,
                        format!("ambiguous .mtl stem: {}", paths.join(", ")),
                    ),
                    StemResolution::Missing => {}
                }
            }
        }
        "cal" | "chrparams" if closure_anim => {
            let text = match std::str::from_utf8(data) {
                Ok(text) => text,
                Err(error) => {
                    out.warnings.push(format!("{name}: not UTF-8 ({error})"));
                    return out;
                }
            };
            let (base, refs) = if ext == "cal" {
                parse_cal(text)
            } else {
                parse_chrparams(text)
            };
            let dir = parent_dir(name).unwrap_or("");
            for reference in refs {
                let base = if reference.include {
                    None
                } else {
                    base.as_deref().or(Some(dir))
                };
                let mut candidates = Vec::new();
                if let Some(base) = base {
                    if !base.is_empty() {
                        candidates.push(format!("{base}/{}", reference.raw));
                    }
                }
                candidates.push(reference.raw.clone());
                add_candidates(name, &reference.raw, &candidates, sources, &mut out);
            }
        }
        _ => {}
    }
    out
}

fn add_path_reference(referenced_by: &str, raw: &str, sources: &SourcePaks, out: &mut Collected) {
    let Some(normalized) = normalize_ref(raw) else {
        push_missing(&mut out.missing, referenced_by, raw, "invalid path".into());
        return;
    };
    add_candidates(
        referenced_by,
        raw,
        std::slice::from_ref(&normalized),
        sources,
        out,
    );
}

fn add_candidates(
    referenced_by: &str,
    raw: &str,
    candidates: &[String],
    sources: &SourcePaks,
    out: &mut Collected,
) {
    let normalized: Vec<String> = candidates
        .iter()
        .filter_map(|candidate| normalize_ref(candidate))
        .collect();
    if normalized.is_empty() {
        push_missing(&mut out.missing, referenced_by, raw, "invalid path".into());
        return;
    }
    let reference = Reference {
        raw: raw.to_string(),
        candidates: normalized,
    };
    resolve_reference(
        sources,
        referenced_by,
        &reference,
        &mut out.resolved,
        &mut out.missing,
    );
}

/// `File`/`Material`/`Model` attribute values of an XML document.
fn xml_attribute_refs(text: &str, attributes: &[&str]) -> std::result::Result<Vec<String>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let doc = Document::parse(text).map_err(|error| format!("XML parse error: {error}"))?;
    let mut refs = Vec::new();
    for node in doc.descendants() {
        for attribute in node.attributes() {
            if attributes
                .iter()
                .any(|name| attribute.name().eq_ignore_ascii_case(name))
            {
                let value = attribute.value().trim();
                if !value.is_empty() && !refs.iter().any(|r: &String| r == value) {
                    refs.push(value.to_string());
                }
            }
        }
    }
    Ok(refs)
}

struct AnimationRef {
    raw: String,
    include: bool,
}

/// Parses `.cal` animation lists: `#filepath`, `$Include` and `.caf`/`.dba` values.
fn parse_cal(text: &str) -> (Option<String>, Vec<AnimationRef>) {
    let mut base = None;
    let mut refs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("--") || line.starts_with(';') {
            continue;
        }
        if line.starts_with('#') && !line.to_ascii_lowercase().starts_with("#filepath") {
            continue;
        }
        let line = line.split("//").next().unwrap_or(line).trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((key, value)) => (key.trim(), value.trim()),
            None => ("", line),
        };
        if key.eq_ignore_ascii_case("#filepath")
            || key.eq_ignore_ascii_case("filepath")
            || key.eq_ignore_ascii_case("path")
        {
            base = normalize_ref(value).or(base);
            continue;
        }
        if key.eq_ignore_ascii_case("$include") || key.eq_ignore_ascii_case("include") {
            if let Some(path) = normalize_ref(value) {
                if has_extension(&path, &["cal", "chrparams"]) {
                    refs.push(AnimationRef {
                        raw: path,
                        include: true,
                    });
                }
            }
            continue;
        }
        for token in value.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
            if let Some(path) = normalize_ref(token) {
                if has_extension(&path, &["caf", "dba"]) {
                    refs.push(AnimationRef {
                        raw: path,
                        include: false,
                    });
                }
            }
        }
    }
    dedupe_animation_refs(&mut refs);
    (base, refs)
}

fn parse_chrparams(text: &str) -> (Option<String>, Vec<AnimationRef>) {
    let mut base = None;
    let mut refs = Vec::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if let Ok(doc) = Document::parse(text) {
        for node in doc.descendants() {
            for attribute in node.attributes() {
                let name = attribute.name();
                let value = attribute.value().trim();
                if name.eq_ignore_ascii_case("path")
                    || name.eq_ignore_ascii_case("filepath")
                    || name.eq_ignore_ascii_case("base")
                {
                    base = normalize_ref(value).or(base);
                    continue;
                }
                scan_animation_value(value, &mut refs);
            }
            if node.is_text() {
                if let Some(value) = node.text() {
                    scan_animation_value(value, &mut refs);
                }
            }
        }
    } else {
        let (parsed_base, parsed_refs) = parse_cal(text);
        base = parsed_base;
        refs = parsed_refs;
    }
    dedupe_animation_refs(&mut refs);
    (base, refs)
}

fn scan_animation_value(value: &str, refs: &mut Vec<AnimationRef>) {
    for token in value.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        if let Some(path) = normalize_ref(token) {
            if has_extension(&path, &["caf", "dba"]) {
                refs.push(AnimationRef {
                    raw: path,
                    include: false,
                });
            } else if has_extension(&path, &["cal", "chrparams"]) {
                refs.push(AnimationRef {
                    raw: path,
                    include: true,
                });
            }
        }
    }
}

fn dedupe_animation_refs(refs: &mut Vec<AnimationRef>) {
    let mut seen = HashSet::new();
    refs.retain(|reference| seen.insert((reference.raw.to_ascii_lowercase(), reference.include)));
}

/// Printable strings in a binary model: embedded material/texture paths and bare material-name
/// candidates. Only strings that contain a path separator and a known material/texture extension
/// are treated as paths; bare names are resolved later and only when a single `.mtl` has that stem.
fn embedded_refs(data: &[u8]) -> (Vec<String>, Vec<String>) {
    let mut paths: Vec<String> = Vec::new();
    let mut tokens: Vec<String> = Vec::new();
    let mut run = Vec::new();
    for &byte in data.iter().chain(std::iter::once(&0u8)) {
        if (0x20..=0x7e).contains(&byte) {
            run.push(byte);
            continue;
        }
        if !run.is_empty() {
            let text = String::from_utf8_lossy(&run);
            for token in text.split_ascii_whitespace() {
                let token = token.trim_matches(|c| {
                    matches!(
                        c,
                        '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}'
                    )
                });
                if token.contains(['/', '\\']) && has_extension(token, TEXTURE_EXTENSIONS) {
                    if !paths.iter().any(|p| p.eq_ignore_ascii_case(token)) {
                        paths.push(token.replace('\\', "/"));
                    }
                } else if is_material_stem_candidate(token)
                    && !tokens.iter().any(|t| t.eq_ignore_ascii_case(token))
                {
                    tokens.push(token.to_string());
                }
            }
            run.clear();
        }
    }
    (paths, tokens)
}

fn is_material_stem_candidate(token: &str) -> bool {
    (6..=96).contains(&token.len())
        && token.contains('_')
        && token.starts_with(|c: char| c.is_ascii_alphabetic())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A bare material name is only trusted when it belongs to the model's asset family (the first
/// `_`-separated component, i.e. the race/creature prefix: `fe_m_...` -> `fe`). Character `.chr`
/// files commonly carry material names for several races; without this check a Dwarf body part
/// would pull Hariharan and Nuian body materials.
fn shares_asset_prefix(model_name: &str, token: &str) -> bool {
    let stem = file_stem(model_name).unwrap_or(model_name);
    let Some(prefix) = stem.split('_').next() else {
        return false;
    };
    token
        .split('_')
        .next()
        .is_some_and(|token_prefix| token_prefix.eq_ignore_ascii_case(prefix))
}

// ---------------------------------------------------------------------------------------------
// build
// ---------------------------------------------------------------------------------------------

struct Selected {
    name: String,
    source: usize,
    added_by: Option<String>,
}

struct BuildSummary {
    written: usize,
    written_bytes: u64,
    new: usize,
    changed: usize,
    skipped_identical: usize,
    missing: Vec<MissingRef>,
    report: Value,
}

fn build(options: &Options) -> Result<BuildSummary> {
    let sources = SourcePaks::open(&options.sources)?;
    let excludes = SourcePaks::open(&options.excludes)?;
    let list_text = fs::read_to_string(&options.list)
        .with_context(|| format!("reading list {}", options.list.display()))?;
    let listed = parse_list(&list_text);
    if listed.is_empty() {
        bail!("list {} names no files", options.list.display());
    }

    let mut selected: BTreeMap<String, Selected> = BTreeMap::new();
    let mut queue: VecDeque<Selected> = VecDeque::new();
    let mut processed = HashSet::new();
    let mut missing = Vec::new();
    let mut warnings = Vec::new();

    for raw in listed {
        let (source, name) = sources
            .resolve(std::slice::from_ref(&raw))
            .with_context(|| {
                format!(
                    "listed file not found: {raw} (checked {} sources)",
                    options.sources.len()
                )
            })?;
        let key = name.to_ascii_lowercase();
        if selected.contains_key(&key) {
            continue;
        }
        selected.insert(
            key,
            Selected {
                name: name.clone(),
                source,
                added_by: None,
            },
        );
        queue.push_back(Selected {
            name,
            source,
            added_by: None,
        });
    }

    if options.closure {
        while let Some(item) = queue.pop_front() {
            if !processed.insert(item.name.to_ascii_lowercase()) {
                continue;
            }
            let ext = extension(&item.name);
            if !matches!(
                ext.as_str(),
                "cdf" | "mtl" | "chr" | "cgf" | "skin" | "cal" | "chrparams"
            ) {
                continue;
            }
            if matches!(ext.as_str(), "cal" | "chrparams") && !options.closure_anim {
                continue;
            }
            let data = sources.read(item.source, &item.name)?;
            let collected = collect_references(&item.name, &data, &sources, options.closure_anim);
            warnings.extend(collected.warnings);
            missing.extend(collected.missing);
            for resolved in collected.resolved {
                let key = resolved.name.to_ascii_lowercase();
                if selected.contains_key(&key) {
                    continue;
                }
                selected.insert(
                    key,
                    Selected {
                        name: resolved.name.clone(),
                        source: resolved.source,
                        added_by: Some(item.name.clone()),
                    },
                );
                queue.push_back(Selected {
                    name: resolved.name,
                    source: resolved.source,
                    added_by: Some(item.name.clone()),
                });
            }
        }
    }

    if let Some(parent) = options.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }
    let mut out_pak = AAPak::create(&options.out)
        .with_context(|| format!("creating overlay {}", options.out.display()))?;
    if out_pak.pak_type() != PakFileType::TypeA {
        bail!(
            "writer produced {:?}, expected TypeA/WIBO (the 1.2 layout)",
            out_pak.pak_type()
        );
    }

    let mut files_report = Vec::new();
    let mut skipped = Vec::new();
    let mut written = 0usize;
    let mut written_bytes = 0u64;
    let mut new = 0usize;
    let mut changed = 0usize;

    for selected in selected.values() {
        let data = sources.read(selected.source, &selected.name)?;
        let size = data.len() as u64;
        let mut status = "new";
        let mut present_in: Option<String> = None;
        for exclude in &excludes.paks {
            let Some(exclude_name) = resolve_in_pak(exclude, &selected.name) else {
                continue;
            };
            status = "changed";
            if present_in.is_none() {
                present_in = Some(exclude.path.display().to_string());
            }
            let other = exclude.pak.read_file(&exclude_name)?;
            if other.as_deref() == Some(data.as_slice()) {
                status = "identical";
                break;
            }
        }
        if status == "identical" {
            skipped.push(json!({
                "name": selected.name,
                "size": size,
                "source": sources.paks[selected.source].path.display().to_string(),
                "present_in": present_in,
            }));
            continue;
        }
        let info = sources.paks[selected.source]
            .pak
            .get_file_by_name(&selected.name)
            .with_context(|| format!("{} disappeared from its source", selected.name))?;
        let create_time = filetime_to_date_time(info.create_time).unwrap_or_else(Utc::now);
        let modify_time = filetime_to_date_time(info.modify_time).unwrap_or_else(Utc::now);
        out_pak
            .add_as_new_file(
                &selected.name,
                &mut Cursor::new(data),
                create_time,
                modify_time,
                false,
            )
            .with_context(|| format!("writing {}", selected.name))?;
        match status {
            "changed" => changed += 1,
            _ => new += 1,
        }
        written += 1;
        written_bytes += size;
        files_report.push(json!({
            "name": selected.name,
            "size": size,
            "source": sources.paks[selected.source].path.display().to_string(),
            "status": status,
            "present_in": present_in,
            "added_by": selected.added_by,
        }));
    }
    out_pak.close()?;
    let out_size = fs::metadata(&options.out)?.len();

    let report = json!({
        "tool": "cargo xtask overlay-pak",
        "out": options.out.display().to_string(),
        "out_size": out_size,
        "header": "TypeA (WIBO)",
        "file_count": written,
        "total_bytes": written_bytes,
        "summary": {
            "included": written,
            "included_new": new,
            "included_changed": changed,
            "identical_skipped": skipped.len(),
            "missing_references": missing.len(),
        },
        "sources": sources.paks.iter().map(|pak| json!({
            "path": pak.path.display().to_string(),
            "pak_type": format!("{:?}", pak.pak.pak_type()),
            "file_count": pak.pak.file_count(),
        })).collect::<Vec<_>>(),
        "exclude_present_in": excludes.paks.iter().map(|pak| json!({
            "path": pak.path.display().to_string(),
            "pak_type": format!("{:?}", pak.pak.pak_type()),
            "file_count": pak.pak.file_count(),
        })).collect::<Vec<_>>(),
        "files": files_report,
        "skipped": skipped,
        "missing_references": missing.iter().map(|item| json!({
            "referenced_by": item.referenced_by,
            "reference": item.reference,
            "reason": item.reason,
        })).collect::<Vec<_>>(),
        "warnings": warnings,
    });

    Ok(BuildSummary {
        written,
        written_bytes,
        new,
        changed,
        skipped_identical: skipped.len(),
        missing,
        report,
    })
}

fn resolve_in_pak(pak: &SourcePak, name: &str) -> Option<String> {
    let normalized = normalize_ref(name)?;
    for form in path_forms(&normalized) {
        if let Some(found) = pak.names.get(&form.to_ascii_lowercase()) {
            return Some(found.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaemu_commons::aapak::AAPak;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "aaemu-overlay-{tag}-{}-{nanos}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn add(pak: &mut AAPak, name: &str, data: &[u8]) {
        pak.add_as_new_file(
            name,
            &mut Cursor::new(data.to_vec()),
            Utc::now(),
            Utc::now(),
            false,
        )
        .unwrap();
    }

    fn write_pak(path: &Path, files: &[(&str, &[u8])]) {
        let mut pak = AAPak::create(path).unwrap();
        for (name, data) in files {
            add(&mut pak, name, data);
        }
        pak.close().unwrap();
    }

    fn list_file(dir: &TempDir, lines: &[&str]) -> PathBuf {
        let path = dir.path().join("list.txt");
        fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn options(
        sources: Vec<PathBuf>,
        list: PathBuf,
        out: PathBuf,
        closure: bool,
        closure_anim: bool,
    ) -> Options {
        Options {
            sources,
            list,
            closure,
            closure_anim,
            excludes: Vec::new(),
            out,
            report: None,
        }
    }

    fn overlay_names(path: &Path) -> Vec<String> {
        let pak = AAPak::open(path, true).unwrap();
        assert_eq!(pak.pak_type(), PakFileType::TypeA);
        let mut names: Vec<_> = pak.file_names().map(str::to_string).collect();
        names.sort();
        names
    }

    #[test]
    fn list_parsing_and_path_normalisation() {
        let list = parse_list(
            "\u{feff}# comment\r\n\r\ngame\\Objects\\A.CDF   \r\n  objects/b.chr # trailing\r\n\
             \"game/objects/c.mtl\"\r\n",
        );
        assert_eq!(
            list,
            vec!["game/Objects/A.CDF", "objects/b.chr", "game/objects/c.mtl"]
        );
        assert_eq!(
            normalize_ref("  game\\\\x\\y.dds ").unwrap(),
            "game/x/y.dds"
        );
        assert!(normalize_ref("../x").is_none());
        assert!(normalize_ref("").is_none());
        assert_eq!(
            path_forms("objects/x.cdf"),
            vec!["objects/x.cdf", "game/objects/x.cdf"]
        );
        assert_eq!(
            path_forms("game/objects/x.cdf"),
            vec!["game/objects/x.cdf", "objects/x.cdf"]
        );
        assert_eq!(
            sibling_mtl("game/a/b/c.chr").as_deref(),
            Some("game/a/b/c.mtl")
        );
        assert_eq!(file_stem("game/a/b/c.dds"), Some("c"));
        assert_eq!(extension("A.DDS"), "dds");
        assert!(shares_asset_prefix(
            "game/objects/characters/dwarf/female/parts/hm/obj/dw_f_1hm133.chr",
            "dw_f_nude"
        ));
        assert!(!shares_asset_prefix(
            "game/objects/characters/dwarf/female/parts/hm/obj/dw_f_1hm133.chr",
            "ha_f_nude"
        ));
    }

    fn synthetic_closure_pak(path: &Path) {
        let cdf = br#"<CharacterDefinition>
            <Model File="objects\Test\part.chr" Material="objects/test/part.mtl"/>
        </CharacterDefinition>"#;
        let mtl = br#"<Material>
            <SubMaterials>
              <Material Name="SubMtl1">
                <Textures>
                  <Texture Map="Diffuse" File="objects/test/part_df.dds"/>
                  <Texture Map="Normalmap" File="game/objects/test/part_ddn.dds"/>
                </Textures>
              </Material>
            </SubMaterials>
        </Material>"#;
        let inc_cal = b"#filepath = animations/test\nidle = idle.caf\n";
        let cal =
            b"#filepath = animations/test\nwalk = walk.caf\n$Include = objects/test/inc.cal\n";
        let embedded_chr =
            b"\0\0game/objects/test/embedded.mtl\0game/objects/test/embedded_df.dds\0\
            shared_mat\0other_mat\0\0";
        let embedded_mtl = br#"<Material><Textures>
            <Texture Map="Diffuse" File="objects/test/embedded_df.dds"/>
            <Texture Map="Specular" File="objects/test/embedded_sp.dds"/>
        </Textures></Material>"#;
        let shared_mtl = br#"<Material><Textures>
            <Texture Map="Diffuse" File="objects/test/shared_df.dds"/>
        </Textures></Material>"#;
        write_pak(
            path,
            &[
                ("game/objects/test/part.cdf", cdf),
                ("game/objects/test/part.chr", b"\0part\0"),
                ("game/objects/test/part.mtl", mtl),
                ("game/objects/test/part_df.dds", b"df"),
                ("game/objects/test/part_ddn.dds", b"ddn"),
                ("game/objects/test/embedded.chr", embedded_chr),
                ("game/objects/test/embedded.mtl", embedded_mtl),
                ("game/objects/test/embedded_df.dds", b"embedded-df"),
                ("game/objects/test/embedded_sp.dds", b"embedded-sp"),
                ("game/objects/test/shared.chr", b"\0shared_mat\0"),
                ("game/objects/test/mat/shared_mat.mtl", shared_mtl),
                ("game/objects/test/mat/other_mat.mtl", b"<Material/>"),
                ("game/objects/test/shared_df.dds", b"shared-df"),
                ("game/animations/test/anim.cal", cal),
                ("game/animations/test/walk.caf", b"walk"),
                ("game/objects/test/inc.cal", inc_cal),
                ("game/animations/test/idle.caf", b"idle"),
            ],
        );
    }

    #[test]
    fn cdf_and_mtl_closure() {
        let source = br#"<CharacterDefinition>
            <Model File="objects\Test\part.chr" Material="objects/test/part.mtl"/>
        </CharacterDefinition>"#;
        let refs = xml_attribute_refs(std::str::from_utf8(source).unwrap(), &["File", "Material"])
            .unwrap();
        assert_eq!(
            refs,
            vec!["objects\\Test\\part.chr", "objects/test/part.mtl"]
        );

        let mtl = br#"<Material><SubMaterials><Material>
            <Textures>
              <Texture Map="Diffuse" File="objects/test/a_df.dds"/>
              <Texture Map="Normalmap" File="game/objects/test/a_ddn.dds"/>
              <Texture Map="Specular" File="objects/test/a_sp.dds"/>
            </Textures>
        </Material></SubMaterials></Material>"#;
        let texture_refs =
            xml_attribute_refs(std::str::from_utf8(mtl).unwrap(), &["File"]).unwrap();
        assert_eq!(
            texture_refs,
            vec![
                "objects/test/a_df.dds",
                "game/objects/test/a_ddn.dds",
                "objects/test/a_sp.dds"
            ]
        );
    }

    #[test]
    fn closure_builds_cdf_mtl_and_binary_references() {
        let dir = TempDir::new("closure");
        let source = dir.path().join("source.pak");
        synthetic_closure_pak(&source);
        let list = list_file(
            &dir,
            &[
                "game/objects/test/part.cdf",
                "game/objects/test/embedded.chr",
                "game/objects/test/shared.chr",
            ],
        );
        let out = dir.path().join("overlay.pak");
        let options = options(vec![source], list, out.clone(), true, false);
        let summary = build(&options).unwrap();
        let names = overlay_names(&out);
        for expected in [
            "game/objects/test/part.cdf",
            "game/objects/test/part.chr",
            "game/objects/test/part.mtl",
            "game/objects/test/part_df.dds",
            "game/objects/test/part_ddn.dds",
            "game/objects/test/embedded.chr",
            "game/objects/test/embedded.mtl",
            "game/objects/test/embedded_df.dds",
            "game/objects/test/embedded_sp.dds",
            "game/objects/test/shared.chr",
            "game/objects/test/mat/shared_mat.mtl",
            "game/objects/test/shared_df.dds",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing {expected}: {names:?}"
            );
        }
        assert!(
            !names
                .iter()
                .any(|n| n == "game/objects/test/mat/other_mat.mtl"),
            "cross-prefix material must not be pulled in: {names:?}"
        );
        assert_eq!(summary.written, 12);
        assert!(summary.missing.is_empty(), "{:?}", summary.missing);
    }

    #[test]
    fn animation_closure_needs_the_flag() {
        let dir = TempDir::new("anim");
        let source = dir.path().join("source.pak");
        synthetic_closure_pak(&source);
        let list = list_file(&dir, &["game/animations/test/anim.cal"]);
        let out_plain = dir.path().join("plain.pak");
        build(&options(
            vec![source.clone()],
            list.clone(),
            out_plain.clone(),
            true,
            false,
        ))
        .unwrap();
        assert_eq!(
            overlay_names(&out_plain),
            vec!["game/animations/test/anim.cal"]
        );

        let out_anim = dir.path().join("anim.pak");
        let summary = build(&options(vec![source], list, out_anim.clone(), true, true)).unwrap();
        let names = overlay_names(&out_anim);
        assert!(names.iter().any(|n| n == "game/animations/test/walk.caf"));
        assert!(names.iter().any(|n| n == "game/objects/test/inc.cal"));
        assert!(names.iter().any(|n| n == "game/animations/test/idle.caf"));
        assert_eq!(summary.written, 4);
    }

    #[test]
    fn embedded_binary_refs_parse_paths_and_unique_stems_only() {
        let data = b"\0game/objects/a.mtl\0game/objects/a_df.dds\0not_an_mtl_stem\0\
            shared_mat\0Bip01 Pelvis\0";
        let (paths, tokens) = embedded_refs(data);
        assert_eq!(paths, vec!["game/objects/a.mtl", "game/objects/a_df.dds"]);
        assert_eq!(tokens, vec!["not_an_mtl_stem", "shared_mat"]);
    }

    #[test]
    fn exclude_present_in_skips_identical_and_keeps_changed() {
        let dir = TempDir::new("exclude");
        let source = dir.path().join("source.pak");
        let exclude = dir.path().join("stock.pak");
        write_pak(
            &source,
            &[
                ("game/a/same.dds", b"same"),
                ("game/a/changed.dds", b"new-data"),
                ("game/a/new.dds", b"new"),
            ],
        );
        write_pak(
            &exclude,
            &[("game/a/same.dds", b"same"), ("game/a/changed.dds", b"old")],
        );
        let list = list_file(
            &dir,
            &["game/a/same.dds", "game/a/changed.dds", "game/a/new.dds"],
        );
        let out = dir.path().join("overlay.pak");
        let mut options = options(vec![source], list, out.clone(), false, false);
        options.excludes = vec![exclude];
        let summary = build(&options).unwrap();
        assert_eq!(summary.written, 2);
        assert_eq!(summary.new, 1);
        assert_eq!(summary.changed, 1);
        assert_eq!(summary.skipped_identical, 1);
        let names = overlay_names(&out);
        assert_eq!(names, vec!["game/a/changed.dds", "game/a/new.dds"]);
        let pak = AAPak::open(&out, true).unwrap();
        assert_eq!(
            pak.read_file("game/a/changed.dds").unwrap().unwrap(),
            b"new-data"
        );
    }

    #[test]
    fn first_source_wins() {
        let dir = TempDir::new("priority");
        let source1 = dir.path().join("source1.pak");
        let source2 = dir.path().join("source2.pak");
        write_pak(&source1, &[("game/a/x.dds", b"first")]);
        write_pak(&source2, &[("game/a/x.dds", b"second")]);
        let list = list_file(&dir, &["a/x.dds"]);
        let out = dir.path().join("overlay.pak");
        build(&options(
            vec![source1, source2],
            list,
            out.clone(),
            false,
            false,
        ))
        .unwrap();
        let overlay = AAPak::open(&out, true).unwrap();
        assert_eq!(
            overlay.read_file("game/a/x.dds").unwrap().unwrap(),
            b"first"
        );
    }

    #[test]
    fn overlay_round_trip_compares_bytes_and_header() {
        let dir = TempDir::new("roundtrip");
        let source = dir.path().join("source.pak");
        let files = [
            ("game/objects/x.cdf", &b"<CharacterDefinition/>"[..]),
            ("game/objects/x.mtl", &b"<Material/>"[..]),
            ("game/objects/x.bin", &(0..=255u8).collect::<Vec<_>>()[..]),
        ];
        write_pak(&source, &files);
        let list = list_file(
            &dir,
            &["objects/x.cdf", "OBJECTS/X.MTL", "game/objects/x.bin"],
        );
        let out = dir.path().join("overlay.pak");
        let summary = build(&options(
            vec![source.clone()],
            list,
            out.clone(),
            false,
            false,
        ))
        .unwrap();
        assert_eq!(summary.written, 3);
        let source_pak = AAPak::open(&source, true).unwrap();
        let overlay = AAPak::open(&out, true).unwrap();
        assert_eq!(overlay.pak_type(), PakFileType::TypeA);
        for (name, _) in files {
            assert_eq!(
                overlay.read_file(name).unwrap().unwrap(),
                source_pak.read_file(name).unwrap().unwrap()
            );
        }
        assert_eq!(overlay.file_count(), 3);
    }

    /// Real 3.0 smoke test: `AAEMU_PAK_3_0` points at the 3.0.4.2 `game_pak`.
    #[test]
    #[ignore = "needs AAEMU_PAK_3_0 pointing at the 3.0.4.2 game_pak"]
    fn real_3_0_overlay_smoke() {
        let Ok(pak_path) = std::env::var("AAEMU_PAK_3_0") else {
            eprintln!("AAEMU_PAK_3_0 not set; skipping");
            return;
        };
        let dir = TempDir::new("real30");
        let list = list_file(
            &dir,
            &[
                "game/objects/characters/dwarf/female/parts/hm/cloth/hm_cloth220/dw_f_hm_cloth220.cdf",
                "game/objects/characters/monster/snowlion/snowlion_armor02/snowlion_helmet02/snowlion_hm_02.cdf",
                "game/objects/characters/dwarf/male/costume/dw_m_sk_china_combat003/dw_m_sk_china_combat003.cdf",
            ],
        );
        let out = dir.path().join("overlay-3.0.pak");
        let options = options(
            vec![PathBuf::from(&pak_path)],
            list,
            out.clone(),
            true,
            false,
        );
        let summary = build(&options).unwrap();
        let overlay = AAPak::open(&out, true).unwrap();
        assert_eq!(overlay.pak_type(), PakFileType::TypeA);
        assert!(summary.written >= 3);
        assert!(summary.written_bytes > 0);
        let source = AAPak::open(&pak_path, true).unwrap();
        for name in overlay.file_names() {
            assert_eq!(
                overlay.read_file(name).unwrap().unwrap(),
                source.read_file(name).unwrap().unwrap(),
                "{name}"
            );
        }
        println!(
            "3.0 overlay: {} files, {} bytes (out {} bytes), missing refs {}",
            summary.written,
            summary.written_bytes,
            fs::metadata(&out).unwrap().len(),
            summary.missing.len()
        );
        assert!(summary.missing.is_empty(), "{:?}", summary.missing);
    }
}
