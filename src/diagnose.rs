//! A bounded, deterministic evidence-gathering plan for an issue or failure report.
//!
//! This is retrieval, not root-cause inference. Trace locations, lexical matches and
//! matching paths supply independent ranking signals. No project code is executed.
//! File names are validated before content is read, but mutable projects are not an OS
//! sandbox: a concurrent rename can invalidate a path check. Use trusted, stable sources.

use crate::{limits, scout, syntax};
use regex::Regex;
use scout::ScoutStatus;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const MAX_REPORT_BYTES: usize = 65_536;
const MAX_TERMS: usize = 24;
const MAX_TERM_BYTES: usize = 160;
const MAX_PATH_BYTES: usize = 1024;
const MAX_FILES: usize = 16;
const MAX_CANDIDATES: usize = 256;
const MAX_CANDIDATE_STORAGE: usize = 160 * 1024;
const MAX_METADATA_BYTES: usize = 256 * 1024;
const MAX_FILE_BYTES: u64 = 400_000;
const MAX_SCAN_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DISPLAY_BYTES: usize = 65_536;
const MAX_TRACE_ANCHORS: usize = 32;
const MAX_LINES_PER_FILE: usize = 3;
const MAX_EVIDENCE_BYTES: usize = 6 * 1024;
const MAX_SCAN_TIME: Duration = Duration::from_secs(8);
const MAX_SYNTAX_TIME: Duration = Duration::from_secs(1);
const NOTICE: &str = "Evidence only: ranking is not confidence or a proven cause; declarations are syntax, not resolved calls.\n";

pub struct Request {
    pub report: String,
    pub terms: Vec<String>,
    pub paths: Vec<String>,
    pub budget: usize,
    pub max_files: usize,
}

pub struct Report {
    pub text: String,
    pub status: ScoutStatus,
    pub structured: Value,
}

#[derive(Default)]
struct Counts {
    candidates: usize,
    read: usize,
    skipped: usize,
    errors: usize,
    matching_files: usize,
    matching_lines: usize,
    ignored_trace_paths: usize,
}

#[derive(Default)]
struct Candidate {
    terms: BTreeSet<usize>,
    lines: Vec<usize>,
    trace_lines: Vec<usize>,
    trace_rank: Option<usize>,
    lexical_rank: Option<usize>,
    path_rank: Option<usize>,
    path_hits: usize,
    matching_lines: usize,
}

impl Candidate {
    fn priority(&self) -> (bool, usize, usize, usize) {
        (
            self.trace_rank.is_some(),
            self.terms.len() + self.path_hits,
            self.path_hits,
            self.matching_lines,
        )
    }

    fn score(&self) -> f64 {
        [self.trace_rank, self.lexical_rank, self.path_rank]
            .into_iter()
            .flatten()
            .map(|rank| 1.0 / (60.0 + rank as f64))
            .sum()
    }
}

fn clipped(text: &str, maximum: usize) -> String {
    let mut end = text.len().min(maximum);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static diagnose expression"))
}

fn valid_relative(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// Reject every symlink component, including links whose current target is inside the
/// root. This keeps the policy explicit and excludes devices/FIFOs before opening.
fn regular_inside(root: &Path, relative: &str) -> Result<PathBuf, &'static str> {
    let path = Path::new(relative);
    if relative.len() > MAX_PATH_BYTES || !valid_relative(path) {
        return Err("invalid or outside path");
    }
    let mut absolute = root.to_path_buf();
    for component in path.components() {
        if let Component::Normal(name) = component {
            absolute.push(name);
            let metadata = std::fs::symlink_metadata(&absolute).map_err(|_| "unreadable path")?;
            if metadata.file_type().is_symlink() {
                return Err("symlink excluded");
            }
        }
    }
    let canonical = absolute.canonicalize().map_err(|_| "unreadable path")?;
    if !canonical.starts_with(root) {
        return Err("outside path");
    }
    if !std::fs::metadata(&canonical)
        .map_err(|_| "unreadable path")?
        .is_file()
    {
        return Err("non-regular file");
    }
    Ok(canonical)
}

fn scopes(root: &Path, requested: &[String]) -> Result<Vec<String>, &'static str> {
    if requested.len() > 128 || requested.iter().map(String::len).sum::<usize>() > 16_384 {
        return Err("too many or oversized paths");
    }
    let mut normalized = Vec::new();
    for name in requested {
        if name.len() > MAX_PATH_BYTES || !valid_relative(Path::new(name)) {
            return Err("paths must stay relative to the project without traversal");
        }
        let mut current = root.to_path_buf();
        for component in Path::new(name).components() {
            if let Component::Normal(part) = component {
                current.push(part);
                let metadata = std::fs::symlink_metadata(&current)
                    .map_err(|_| "requested path could not be inspected")?;
                if metadata.file_type().is_symlink() {
                    return Err("requested path contains a symlink");
                }
            }
        }
        let canonical = current
            .canonicalize()
            .map_err(|_| "requested path could not be inspected")?;
        if !canonical.starts_with(root) {
            return Err("requested path is outside the project");
        }
        normalized.push(name.clone());
    }
    Ok(normalized)
}

fn source_anchors(root: &Path, report: &str) -> (Vec<(String, usize)>, usize, bool) {
    static LOCATIONS: OnceLock<Regex> = OnceLock::new();
    let locations = regex(
        &LOCATIONS,
        r"(?P<path>[A-Za-z0-9_@./+\-]+\.(?:rs|jsx?|tsx?|mjs|cjs|py|go|java|cpp|cc|c|hpp|h|toml|json|ya?ml))(?::(?P<line>[1-9][0-9]{0,8}))?",
    );
    let mut found = Vec::new();
    let mut ignored = 0;
    let mut truncated = false;
    for captures in locations.captures_iter(report) {
        if found.len() == MAX_TRACE_ANCHORS {
            truncated = true;
            break;
        }
        let matched = captures.name("path").unwrap();
        let before = &report[..matched.start()];
        // Never reinterpret the suffix of an unsupported/ambiguous path as a
        // different file. In particular, `src/a:b.rs` is not `b.rs`, and
        // `src/spaced name.rs` is not an unqualified mention of `name.rs`.
        if before.chars().next_back().is_some_and(|ch| {
            !ch.is_whitespace() && !matches!(ch, '(' | '[' | '{' | '<' | '\'' | '"' | '`' | '=')
        }) {
            ignored += 1;
            continue;
        }
        let prior_word = before.split_whitespace().next_back().unwrap_or("");
        if before.ends_with(char::is_whitespace)
            && prior_word.chars().any(|ch| matches!(ch, '/' | '\\'))
        {
            ignored += 1;
            continue;
        }
        let raw = matched.as_str();
        let raw_path = Path::new(raw);
        if raw_path
            .components()
            .any(|part| part == Component::ParentDir)
        {
            ignored += 1;
            continue;
        }
        let relative = if raw_path.is_absolute() {
            match raw_path.strip_prefix(root) {
                Ok(path) => path.to_string_lossy().into_owned(),
                Err(_) => {
                    ignored += 1;
                    continue;
                }
            }
        } else {
            raw_path
                .components()
                .filter_map(|part| match part {
                    Component::Normal(name) => Some(name),
                    _ => None,
                })
                .collect::<PathBuf>()
                .to_string_lossy()
                .into_owned()
        };
        if regular_inside(root, &relative).is_err() {
            ignored += 1;
            continue;
        }
        let line = captures
            .name("line")
            .and_then(|line| line.as_str().parse::<usize>().ok())
            .unwrap_or(0);
        if !found.iter().any(|item| item == &(relative.clone(), line)) {
            found.push((relative, line));
        }
    }
    (found, ignored, truncated)
}

fn terms(request: &Request, anchors: &[(String, usize)]) -> (Vec<String>, bool) {
    static WORDS: OnceLock<Regex> = OnceLock::new();
    static QUOTED: OnceLock<Regex> = OnceLock::new();
    const STOP: &[&str] = &[
        "about",
        "after",
        "again",
        "also",
        "been",
        "before",
        "being",
        "between",
        "both",
        "could",
        "does",
        "each",
        "error",
        "expected",
        "failed",
        "failure",
        "file",
        "files",
        "from",
        "function",
        "have",
        "here",
        "into",
        "issue",
        "just",
        "like",
        "line",
        "more",
        "must",
        "name",
        "need",
        "only",
        "other",
        "return",
        "same",
        "should",
        "some",
        "test",
        "tests",
        "than",
        "that",
        "their",
        "them",
        "then",
        "there",
        "these",
        "they",
        "this",
        "through",
        "using",
        "value",
        "want",
        "were",
        "what",
        "when",
        "where",
        "which",
        "while",
        "will",
        "with",
        "without",
        "would",
        "your",
        "true",
        "false",
        "null",
        "undefined",
    ];
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    let mut add = |term: &str| {
        let term = term.trim();
        if !term.is_empty() && term.len() <= MAX_TERM_BYTES && seen.insert(term.to_lowercase()) {
            selected.push(term.to_string());
        }
    };
    for term in &request.terms {
        add(term);
    }
    for (path, _) in anchors {
        if let Some(stem) = Path::new(path).file_stem().and_then(|stem| stem.to_str()) {
            add(stem);
        }
    }
    // Short quoted identifiers and messages are more useful than generic issue prose.
    let quoted = regex(&QUOTED, r#"[`'"]([^`'"\n]{3,160})[`'"]"#);
    for captures in quoted.captures_iter(&request.report) {
        add(&captures[1]);
    }
    let mut words = BTreeMap::<String, (String, usize, usize, usize)>::new();
    for (index, found) in regex(&WORDS, r"[\p{L}_][\p{L}\p{N}_]{2,159}")
        .find_iter(&request.report)
        .enumerate()
    {
        let word = found.as_str();
        let key = word.to_lowercase();
        if STOP.contains(&key.as_str()) || word.len() > MAX_TERM_BYTES {
            continue;
        }
        let distinctive = usize::from(word.contains('_')) * 4
            + usize::from(word.chars().skip(1).any(char::is_uppercase)) * 3
            + usize::from(word.chars().any(char::is_numeric)) * 2;
        let entry = words
            .entry(key)
            .or_insert((word.to_string(), 0, distinctive, index));
        entry.1 += 1;
    }
    let mut words: Vec<_> = words.into_values().collect();
    words.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then(right.1.cmp(&left.1))
            .then(left.3.cmp(&right.3))
    });
    for (word, _, _, _) in words {
        add(&word);
    }
    let truncated = selected.len() > MAX_TERMS;
    selected.truncate(MAX_TERMS);
    (selected, truncated)
}

fn retain_candidate(
    candidates: &mut BTreeMap<String, Candidate>,
    path: &str,
    mut candidate: Candidate,
    storage: &mut usize,
    truncated: &mut bool,
) {
    if let Some(existing) = candidates.get_mut(path) {
        existing.terms.append(&mut candidate.terms);
        existing.matching_lines += candidate.matching_lines;
        existing.path_hits = existing.path_hits.max(candidate.path_hits);
        for line in candidate.lines {
            if existing.lines.len() < MAX_LINES_PER_FILE && !existing.lines.contains(&line) {
                existing.lines.push(line);
            }
        }
        for line in candidate.trace_lines {
            if existing.trace_lines.len() < MAX_LINES_PER_FILE
                && !existing.trace_lines.contains(&line)
            {
                existing.trace_lines.push(line);
            } else if !existing.trace_lines.contains(&line) {
                *truncated = true;
            }
        }
        existing.trace_rank = existing.trace_rank.or(candidate.trace_rank);
        return;
    }
    // Retain complete per-file summaries, replacing a weaker unanchored candidate.
    // Discovery order must not hide a late file matching several distinct terms.
    let added = path.len() + 1024;
    while candidates.len() >= MAX_CANDIDATES
        || storage.saturating_add(added) > MAX_CANDIDATE_STORAGE
    {
        *truncated = true;
        let weakest = candidates
            .iter()
            .filter(|(_, value)| value.trace_rank.is_none())
            .min_by(|left, right| {
                left.1
                    .priority()
                    .cmp(&right.1.priority())
                    .then(right.0.cmp(left.0))
            })
            .map(|(name, value)| (name.clone(), value.priority()));
        let Some((name, priority)) = weakest else {
            return;
        };
        if candidate.priority() < priority
            || (candidate.priority() == priority && path >= name.as_str())
        {
            return;
        }
        if candidates.remove(&name).is_some() {
            *storage = storage.saturating_sub(name.len() + 1024);
        } else {
            *truncated = true;
            return;
        }
    }
    *storage += added;
    candidates.insert(path.to_string(), candidate);
}

fn counts_json(counts: &Counts, selected: usize, anchors: usize) -> Value {
    json!({
        "candidates":counts.candidates,"read":counts.read,"skipped":counts.skipped,
        "errors":counts.errors,"matching_files":counts.matching_files,
        "matching_lines":counts.matching_lines,"trace_anchors":anchors,
        "ignored_trace_paths":counts.ignored_trace_paths,"selected_files":selected,
    })
}

fn base(root: &Path, status: ScoutStatus, reason: Option<&str>) -> Value {
    json!({
        "schema_version":1,"tool":"diagnose","cwd":clipped(&root.to_string_lossy(),MAX_PATH_BYTES),
        "status":status.as_str(),"reason":reason,"terms":[],
        "counts":counts_json(&Counts::default(),0,0),"candidates":[],
        "metadata_truncated":root.to_string_lossy().len()>MAX_PATH_BYTES,"display_truncated":false,
        "selection_truncated":false,"terms_truncated":false,
        "limits":{"report_bytes":MAX_REPORT_BYTES,"file_bytes":MAX_FILE_BYTES,
            "scan_bytes":MAX_SCAN_BYTES,"display_bytes":MAX_DISPLAY_BYTES,
            "metadata_bytes":MAX_METADATA_BYTES,"candidate_files":MAX_CANDIDATES,
            "selected_files":MAX_FILES,"scan_milliseconds":MAX_SCAN_TIME.as_millis() as u64},
        "notes":[NOTICE.trim(),"Static path confinement is not an OS sandbox; use stable, trusted sources."]
    })
}

fn error(root: &Path, reason: &str, budget: usize) -> Report {
    let text = clipped(reason, budget.min(MAX_DISPLAY_BYTES));
    let mut structured = base(root, ScoutStatus::Error, Some(reason));
    structured["display_truncated"] = json!(text.len() < reason.len());
    Report {
        text,
        status: ScoutStatus::Error,
        structured,
    }
}

fn rank(candidates: &mut BTreeMap<String, Candidate>, terms: &[String]) {
    let mut lexical: Vec<_> = candidates
        .iter()
        .filter(|(_, candidate)| !candidate.terms.is_empty())
        .map(|(path, candidate)| {
            (
                path.clone(),
                candidate.terms.len(),
                candidate.matching_lines,
            )
        })
        .collect();
    lexical.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then(right.2.cmp(&left.2))
            .then(left.0.cmp(&right.0))
    });
    for (index, (path, _, _)) in lexical.into_iter().enumerate() {
        candidates.get_mut(&path).unwrap().lexical_rank = Some(index + 1);
    }
    let mut names: Vec<_> = candidates
        .keys()
        .filter_map(|path| {
            let lower = path.to_lowercase();
            let hits = terms
                .iter()
                .filter(|term| lower.contains(&term.to_lowercase()))
                .count();
            (hits > 0).then_some((path.clone(), hits))
        })
        .collect();
    names.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for (index, (path, _)) in names.into_iter().enumerate() {
        candidates.get_mut(&path).unwrap().path_rank = Some(index + 1);
    }
}

fn evidence(
    root: &Path,
    path: &str,
    candidate: &Candidate,
    syntax_remaining: Duration,
) -> Result<(Vec<Value>, bool), &'static str> {
    let absolute = regular_inside(root, path)?;
    let bytes = limits::read_regular_file(&absolute).map_err(|_| "candidate could not be read")?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("candidate exceeds source limit");
    }
    let source = std::str::from_utf8(&bytes).map_err(|_| "candidate is not UTF-8")?;
    let lines: Vec<_> = source.lines().collect();
    if lines.is_empty() {
        return Ok((Vec::new(), false));
    }
    let analysis = syntax::analyze(path, source, syntax_remaining);
    let mut locations = candidate.trace_lines.clone();
    locations.extend(&candidate.lines);
    if locations.is_empty() {
        locations.push(1);
    }
    locations.sort_by_key(|line| (!candidate.trace_lines.contains(line), *line));
    locations.dedup();
    let mut ranges = Vec::<(usize, usize, &'static str)>::new();
    let mut clipped_any = false;
    for line in locations.into_iter().take(MAX_LINES_PER_FILE) {
        if line == 0 || line > lines.len() {
            continue;
        }
        let start = line.saturating_sub(13).max(1);
        let end = line.saturating_add(12).min(lines.len());
        if ranges
            .iter()
            .any(|(old_start, old_end, _)| *old_start <= line && line <= *old_end)
        {
            continue;
        }
        let context = match analysis.context(line) {
            syntax::Context::Found { inner, .. } => {
                if inner.start_line < start {
                    ranges.push((
                        inner.start_line,
                        inner.header_end_line.min(inner.start_line + 7),
                        "syntax_header",
                    ));
                }
                "syntax_window"
            }
            syntax::Context::Ambiguous => "ambiguous_window",
            _ => "lexical_window",
        };
        ranges.push((start, end, context));
    }
    ranges.sort_by_key(|range| (range.0, range.1));
    let mut merged: Vec<(usize, usize, &'static str)> = Vec::new();
    for (start, end, kind) in ranges {
        if let Some(previous) = merged.last_mut().filter(|previous| start <= previous.1 + 1) {
            previous.1 = previous.1.max(end);
            if previous.2 != kind {
                previous.2 = kind;
            }
        } else {
            merged.push((start, end, kind));
        }
    }
    let mut result = Vec::new();
    let mut used = 0;
    for (start, end, kind) in merged {
        let mut text = String::new();
        let mut actual_end = start;
        for (index, line) in lines.iter().enumerate().take(end).skip(start - 1) {
            let remaining = MAX_EVIDENCE_BYTES.saturating_sub(used + text.len());
            if remaining <= 1 {
                clipped_any = true;
                break;
            }
            let shown = clipped(line, remaining.saturating_sub(1).min(320));
            clipped_any |= shown.len() < line.len();
            text.push_str(&shown);
            text.push('\n');
            actual_end = index + 1;
        }
        if text.is_empty() {
            clipped_any = true;
            break;
        }
        used += text.len();
        result.push(json!({"start_line":start,"end_line":actual_end,"text":text,
            "context_kind":kind,"syntax_status":analysis.status.as_str(),"language":analysis.language,
            "text_clipped":clipped_any}));
        if used >= MAX_EVIDENCE_BYTES {
            clipped_any = true;
            break;
        }
    }
    Ok((result, clipped_any))
}

fn render(structured: &Value, budget: usize) -> (String, bool) {
    let mut output = format!(
        "DIAGNOSE — {}\n{NOTICE}",
        structured["status"].as_str().unwrap_or("error")
    );
    if let Some(reason) = structured["reason"].as_str() {
        output.push_str(&format!("[incomplete: {reason}]\n"));
    }
    output.push_str(&format!(
        "Terms: {}\nFiles (ranked):\n",
        structured["terms"]
    ));
    let candidates = structured["candidates"].as_array().unwrap();
    // Reserve every selected file name before spending the budget on detailed excerpts.
    for candidate in candidates {
        output.push_str(&format!("  {}\n", candidate["path"]));
    }
    for candidate in candidates {
        output.push_str(&format!(
            "\n## {}\nSignals: {}\n",
            candidate["path"], candidate["signals"]
        ));
        for excerpt in candidate["evidence"].as_array().unwrap() {
            output.push_str(&format!(
                "[{}; lines {}–{}]\n",
                excerpt["context_kind"], excerpt["start_line"], excerpt["end_line"]
            ));
            let start = excerpt["start_line"].as_u64().unwrap_or(1);
            for (offset, line) in excerpt["text"].as_str().unwrap_or("").lines().enumerate() {
                output.push_str(&format!("{:>5} {line}\n", start + offset as u64));
            }
        }
    }
    let text = clipped(&output, budget.min(MAX_DISPLAY_BYTES));
    let truncated = text.len() < output.len();
    (text, truncated)
}

/// Collect evidence using a fixed plan. Completion concerns the eligible scan, not the
/// truth of an explanation, exhaustive symbol resolution or all evidence fitting on screen.
pub fn diagnose_report(dir: &Path, request: &Request) -> Report {
    if request.report.len() > MAX_REPORT_BYTES
        || request.terms.len() > MAX_TERMS
        || request.terms.iter().any(|term| term.len() > MAX_TERM_BYTES)
        || request.max_files == 0
        || request.max_files > MAX_FILES
    {
        return error(
            dir,
            "diagnose: oversized input or max_files outside 1..16",
            request.budget,
        );
    }
    if request.report.trim().is_empty() && request.terms.iter().all(|term| term.trim().is_empty()) {
        return error(
            dir,
            "diagnose: give useful report terms or explicit terms",
            request.budget,
        );
    }
    let root = match dir.canonicalize() {
        Ok(root) if root.is_dir() => root,
        _ => {
            return error(
                dir,
                "diagnose: project directory is unavailable",
                request.budget,
            )
        }
    };
    let requested = match scopes(&root, &request.paths) {
        Ok(paths) => paths,
        Err(reason) => return error(&root, reason, request.budget),
    };
    let started = Instant::now();
    let (anchors, ignored, anchors_truncated) = source_anchors(&root, &request.report);
    let (terms, terms_truncated) = terms(request, &anchors);
    if terms.is_empty() {
        return error(
            &root,
            "diagnose: give useful report terms or explicit terms",
            request.budget,
        );
    }
    let (names, discovery_truncated) = match scout::candidate_paths(&root, &requested) {
        Ok(names) => names,
        Err(_) => return error(&root, "diagnose: file discovery failed", request.budget),
    };
    let allowed: BTreeSet<_> = names.iter().map(String::as_str).collect();
    let mut counts = Counts {
        candidates: names.len(),
        ignored_trace_paths: ignored,
        ..Counts::default()
    };
    let mut candidates = BTreeMap::<String, Candidate>::new();
    let mut storage = 0;
    let mut metadata_truncated = anchors_truncated;
    for (index, (path, line)) in anchors.iter().enumerate() {
        if !allowed.contains(path.as_str()) {
            continue;
        }
        retain_candidate(
            &mut candidates,
            path,
            Candidate {
                trace_rank: Some(index + 1),
                trace_lines: if *line > 0 { vec![*line] } else { Vec::new() },
                ..Candidate::default()
            },
            &mut storage,
            &mut metadata_truncated,
        );
    }
    let mut total_bytes = 0u64;
    let mut paths = Vec::new();
    let mut partial = discovery_truncated;
    for path in names {
        if started.elapsed() >= MAX_SCAN_TIME {
            partial = true;
            break;
        }
        if scout::is_skipped(&path) {
            counts.skipped += 1;
            continue;
        }
        let absolute = match regular_inside(&root, &path) {
            Ok(absolute) => absolute,
            Err("unreadable path") => {
                counts.errors += 1;
                partial = true;
                continue;
            }
            Err(_) => {
                counts.skipped += 1;
                continue;
            }
        };
        let Ok(metadata) = std::fs::metadata(&absolute) else {
            counts.errors += 1;
            partial = true;
            continue;
        };
        if metadata.len() > MAX_FILE_BYTES {
            counts.skipped += 1;
            continue;
        }
        if total_bytes.saturating_add(metadata.len()) > MAX_SCAN_BYTES {
            partial = true;
            break;
        }
        total_bytes += metadata.len();
        paths.push(path);
    }
    // Scout's ordinary per-query path budget is preserved. These are exact validated
    // names, never a second Git pathspec expansion.
    let mut offset = 0;
    while offset < paths.len() {
        if started.elapsed() >= MAX_SCAN_TIME {
            partial = true;
            break;
        }
        let start = offset;
        let mut bytes = 0;
        while offset < paths.len() && offset - start < 128 && bytes + paths[offset].len() <= 16_384
        {
            bytes += paths[offset].len();
            offset += 1;
        }
        if start == offset {
            partial = true;
            break;
        }
        let (scan, summaries) = scout::scout_report_lexical(
            &root,
            &scout::Request {
                terms: terms.clone(),
                paths: paths[start..offset].to_vec(),
                budget: 0,
            },
        );
        counts.read += scan.counts.read;
        counts.skipped += scan.counts.skipped;
        counts.errors += scan.counts.errors;
        counts.matching_files += scan.counts.matching_files;
        counts.matching_lines += scan.counts.matching_lines;
        partial |= scan.status != ScoutStatus::Complete;
        // Public per-line Scout references may truncate on long paths or dense
        // matches. The bounded per-file summaries retain every scanned file's
        // term coverage and first representative line independently of that cap.
        for summary in summaries {
            let path = summary.path;
            let mut candidate = Candidate {
                matching_lines: summary.matching_lines,
                ..Candidate::default()
            };
            let mut representative_lines = BTreeMap::<usize, BTreeSet<usize>>::new();
            for (term, line) in summary.term_lines.into_iter().enumerate() {
                if line > 0 {
                    candidate.terms.insert(term);
                    representative_lines.entry(line).or_default().insert(term);
                }
            }
            let mut covered = BTreeSet::new();
            while candidate.lines.len() < MAX_LINES_PER_FILE && !representative_lines.is_empty() {
                let best = representative_lines
                    .iter()
                    .max_by(|left, right| {
                        left.1
                            .difference(&covered)
                            .count()
                            .cmp(&right.1.difference(&covered).count())
                            .then(right.0.cmp(left.0))
                    })
                    .map(|(line, _)| *line)
                    .unwrap();
                covered.extend(representative_lines.remove(&best).unwrap());
                candidate.lines.push(best);
            }
            let lower = path.to_lowercase();
            candidate.path_hits = terms
                .iter()
                .filter(|term| lower.contains(&term.to_lowercase()))
                .count();
            retain_candidate(
                &mut candidates,
                &path,
                candidate,
                &mut storage,
                &mut metadata_truncated,
            );
        }
    }
    // A path mention or an identifier matching the path is evidence even if the file's
    // content never spells its own name. Do not retain thousands of path-only candidates.
    for path in &paths {
        let lower = path.to_lowercase();
        let path_hits = terms
            .iter()
            .filter(|term| lower.contains(&term.to_lowercase()))
            .count();
        if path_hits > 0 {
            retain_candidate(
                &mut candidates,
                path,
                Candidate {
                    path_hits,
                    ..Candidate::default()
                },
                &mut storage,
                &mut metadata_truncated,
            );
        }
    }
    rank(&mut candidates, &terms);
    let mut ranked: Vec<_> = candidates.into_iter().collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .score()
            .total_cmp(&left.1.score())
            .then(left.0.cmp(&right.0))
    });
    let selection_truncated = ranked.len() > request.max_files;
    ranked.truncate(request.max_files);
    let syntax_started = Instant::now();
    let mut selected = Vec::new();
    let mut encoded = 0;
    for (path, candidate) in ranked {
        let remaining = MAX_SYNTAX_TIME.saturating_sub(syntax_started.elapsed());
        let (excerpts, mut clipped) = match evidence(&root, &path, &candidate, remaining) {
            Ok(value) => value,
            Err(_) => {
                counts.errors += 1;
                partial = true;
                (Vec::new(), false)
            }
        };
        clipped |= candidate.matching_lines > candidate.lines.len();
        let mut value = json!({"path":path,"score":candidate.score(),
            "signals":{"trace_rank":candidate.trace_rank,"lexical_rank":candidate.lexical_rank,
                "path_rank":candidate.path_rank,"term_indices":candidate.terms,
                "trace_lines":candidate.trace_lines,"retained_matching_lines":candidate.matching_lines},
            "evidence":excerpts,"evidence_truncated":clipped});
        let mut size = serde_json::to_vec(&value).map_or(MAX_METADATA_BYTES, |bytes| bytes.len());
        if encoded + size > MAX_METADATA_BYTES - 65_536 {
            value["evidence"] = json!([]);
            value["evidence_truncated"] = json!(true);
            metadata_truncated = true;
            size = serde_json::to_vec(&value).map_or(MAX_METADATA_BYTES, |bytes| bytes.len());
        }
        encoded += size;
        selected.push(value);
    }
    let status = if partial {
        ScoutStatus::Partial
    } else {
        ScoutStatus::Complete
    };
    let reason =
        partial.then_some("some candidates could not be scanned or a resource limit was reached");
    let mut structured = base(&root, status, reason);
    structured["terms"] = json!(terms);
    structured["counts"] = counts_json(
        &counts,
        selected.len(),
        anchors.iter().filter(|(_, line)| *line > 0).count(),
    );
    structured["candidates"] = json!(selected);
    structured["metadata_truncated"] = json!(metadata_truncated);
    structured["selection_truncated"] = json!(selection_truncated);
    structured["terms_truncated"] = json!(terms_truncated);
    // The encoded size, not the raw String sizes, is the contract: control
    // characters can expand sixfold in JSON. Keep all selected file headers and
    // remove detailed excerpts from the lowest-ranked files until it fits.
    while serde_json::to_vec(&structured).is_ok_and(|bytes| bytes.len() > MAX_METADATA_BYTES) {
        let candidate = structured["candidates"].as_array_mut().and_then(|items| {
            items.iter_mut().rev().find(|candidate| {
                candidate["evidence"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
            })
        });
        let Some(candidate) = candidate else {
            // At most 16 headers, bounded paths and 24 bounded terms fit even
            // with maximum JSON escaping; no source excerpts remain here.
            break;
        };
        candidate["evidence"] = json!([]);
        candidate["evidence_truncated"] = json!(true);
        structured["metadata_truncated"] = json!(true);
    }
    let (text, truncated) = render(&structured, request.budget);
    structured["display_truncated"] = json!(truncated);
    Report {
        text,
        status,
        structured,
    }
}

/// Versioned contract shared with the MCP descriptor. The open nested metadata objects
/// permit future additive provenance, while evidence fields and top-level status are typed.
pub fn output_schema() -> Value {
    json!({"type":"object","required":["schema_version","tool","cwd","status","reason","terms","counts","candidates","metadata_truncated","display_truncated","selection_truncated","terms_truncated","limits","notes"],
    "properties":{
        "schema_version":{"const":1},"tool":{"const":"diagnose"},"cwd":{"type":"string"},
        "status":{"enum":["complete","partial","error"]},"reason":{"type":["string","null"]},
        "terms":{"type":"array","maxItems":MAX_TERMS,"items":{"type":"string"}},
        "counts":{"type":"object","required":["candidates","read","skipped","errors","matching_files","matching_lines","trace_anchors","ignored_trace_paths","selected_files"],"additionalProperties":{"type":"integer","minimum":0}},
        "candidates":{"type":"array","maxItems":MAX_FILES,"items":{"type":"object","required":["path","score","signals","evidence","evidence_truncated"],"properties":{
            "path":{"type":"string"},"score":{"type":"number","minimum":0},"signals":{"type":"object"},"evidence_truncated":{"type":"boolean"},
            "evidence":{"type":"array","items":{"type":"object","required":["start_line","end_line","text","context_kind","syntax_status","language","text_clipped"],"properties":{
                "start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1},"text":{"type":"string"},"context_kind":{"enum":["syntax_header","syntax_window","ambiguous_window","lexical_window"]},"syntax_status":{"type":"string"},"language":{"type":["string","null"]},"text_clipped":{"type":"boolean"}}}}
        }}},
        "metadata_truncated":{"type":"boolean"},"display_truncated":{"type":"boolean"},"selection_truncated":{"type":"boolean"},"terms_truncated":{"type":"boolean"},"limits":{"type":"object"},"notes":{"type":"array","items":{"type":"string"}}
    }})
}
