//! Scout: the map of an area of the code, in one call.
//!
//! An AI agent that starts on a task spends its first ten to twenty round trips rebuilding a
//! picture of the code: grep a word, list a directory, grep another word, read a passage.
//! Each of those round trips re-reads the whole conversation. Scout does that exploration in
//! one go, with no model involved: the agent gives the words it cares about, and gets back
//! the files that mention them, the matching lines, and nearby declaration context.
//!
//! It is a text search and says so in its answer: code that does the same thing without the
//! words is not listed.

use crate::syntax;
use regex::bytes::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Files larger than this are skipped: generated or vendored, rarely what an agent wants.
const MAX_FILE_BYTES: u64 = 400_000;
/// Longer lines are clipped in the answer.
const MAX_LINE_BYTES: usize = 200;
/// At most this many terms are searched (one bit each in a `u64`).
const MAX_TERMS: usize = 60;
const MAX_TERM_BYTES: usize = 512;
const MAX_QUERY_BYTES: usize = 16_384;
const MAX_PATHS: usize = 128;
const MAX_LISTING_BYTES: usize = 2 * 1024 * 1024;
const MAX_WALK_ENTRIES: usize = 50_000;
const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;
const MAX_ANSWER_BYTES: usize = 64 * 1024;
/// Bounds for references returned independently of the text display budget.
const MAX_MATCH_RECORDS: usize = 4096;
const MAX_METADATA_BYTES: usize = 256 * 1024;
const MAX_STRUCTURE_FILES: usize = 256;
const MAX_SYNTAX_INPUT: usize = 4 * 1024 * 1024;
const MAX_SYNTAX_RETAINED: usize = 2 * 1024 * 1024;
const MAX_SYNTAX_TIME: Duration = Duration::from_secs(2);
const MAX_SIGNATURE_LINES: usize = 8;
const GIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Matching lines shown for a detailed file, and for a previewed one.
const LINES_PER_FILE: usize = 14;
const LINES_PER_PREVIEW: usize = 2;
/// More-indented lines shown after a matching line: the rest of its statement.
const CONTINUATION_LINES: usize = 4;
/// Share of the best score of a directory that each of its files receives.
const NEIGHBOUR_SHARE: f64 = 0.6;

const SKIPPED_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "dist",
    "build",
    ".next",
    ".venv",
    "__pycache__",
    "vendor",
    ".turbo",
    "coverage",
];
const SKIPPED_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "pdf", "glb", "gltf", "bin", "woff", "woff2",
    "ttf", "otf", "mp3", "mp4", "mov", "zip", "gz", "lock", "map", "svg", "wasm",
];
/// Words that mark the line opening a block as a definition worth showing.
const DEFINITION_WORDS: &[&str] = &[
    "function ",
    "class ",
    "def ",
    "fn ",
    "interface ",
    "type ",
    "struct ",
    "enum ",
    "impl ",
    "func ",
    "const ",
    "export ",
    "module ",
    "trait ",
];

/// What to look for.
pub struct Request {
    /// Words, phrases or identifiers (plain text, case-insensitive).
    pub terms: Vec<String>,
    /// Directories or files to restrict the search to, relative to the project.
    pub paths: Vec<String>,
    /// Maximum size of the answer, in UTF-8 bytes (at most 64 KiB).
    pub budget: usize,
}

/// Whether Scout finished the eligible text search, returned partial evidence, or failed
/// before the scan could begin. `Complete` excludes the documented skipped file kinds;
/// it does not claim semantic coverage or that every match fits in the text/metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScoutStatus {
    Complete,
    Partial,
    Error,
}

impl ScoutStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Error => "error",
        }
    }
}

/// Counts of discovered candidates and outcomes. Matching counts cover all text files
/// actually searched, even when the independent reference/display limits are reached.
#[derive(Default, Debug)]
pub struct ScoutCounts {
    pub candidates: usize,
    pub read: usize,
    pub skipped: usize,
    pub errors: usize,
    pub matching_files: usize,
    pub matching_lines: usize,
}

/// A direct source reference produced during scanning, in discovery order. Lines are
/// one-based; term indices address `ScoutReport::terms` (the normalized search terms).
#[derive(Debug)]
pub struct ScoutMatch {
    pub path: String,
    pub line_number: usize,
    pub term_indices: Vec<usize>,
}

/// Syntax evidence is deduplicated by file. Definitions contain at least one matching
/// line; they are syntactic ranges, not resolved references or unique match ownership.
#[derive(Debug)]
pub struct ScoutFileStructure {
    pub path: String,
    pub language: Option<&'static str>,
    pub status: syntax::Status,
    pub definitions: Vec<syntax::Scope>,
    pub definitions_truncated: bool,
}

/// A bounded machine-readable view of Scout, independent of presentation. `reason` and
/// counts survive even a zero-byte text budget. `metadata_truncated` means that `matches`
/// omits references; `display_truncated` means the rendered text exceeded its byte budget.
/// The normal rendering also selects excerpts and clips long lines as documented in text.
#[derive(Debug)]
pub struct ScoutReport {
    pub text: String,
    pub status: ScoutStatus,
    pub reason: Option<String>,
    pub terms: Vec<String>,
    pub counts: ScoutCounts,
    pub matches: Vec<ScoutMatch>,
    pub metadata_truncated: bool,
    pub display_truncated: bool,
    pub structure: Vec<ScoutFileStructure>,
    pub structure_truncated: bool,
}

impl ScoutReport {
    fn error(reason: String, text: String, budget: usize) -> Self {
        let display_truncated = text.len() > budget;
        Self {
            text: bounded_answer(text, budget),
            status: ScoutStatus::Error,
            reason: Some(reason),
            terms: Vec::new(),
            counts: ScoutCounts::default(),
            matches: Vec::new(),
            metadata_truncated: false,
            display_truncated,
            structure: Vec::new(),
            structure_truncated: false,
        }
    }
}

#[derive(Default)]
struct MatchReferences {
    matches: Vec<ScoutMatch>,
    /// Owned path/term storage, excluding the separately checked records allocation.
    owned_bytes: usize,
    truncated: bool,
}

impl MatchReferences {
    fn allocated_bytes(&self) -> usize {
        self.matches.capacity() * std::mem::size_of::<ScoutMatch>() + self.owned_bytes
    }

    fn add(&mut self, file: &FileHits, budget: usize) {
        for hit in &file.hits {
            if self.truncated {
                break;
            }
            let owned =
                file.path.len() + hit.terms.count_ones() as usize * std::mem::size_of::<usize>();
            let capacity = if self.matches.len() == self.matches.capacity() {
                self.matches
                    .capacity()
                    .max(4)
                    .saturating_mul(2)
                    .min(MAX_MATCH_RECORDS)
            } else {
                self.matches.capacity()
            };
            if self.matches.len() == MAX_MATCH_RECORDS
                || capacity * std::mem::size_of::<ScoutMatch>() + self.owned_bytes + owned > budget
            {
                self.truncated = true;
                break;
            }
            if capacity > self.matches.capacity() {
                if self
                    .matches
                    .try_reserve_exact(capacity - self.matches.len())
                    .is_err()
                {
                    self.truncated = true;
                    break;
                }
                if self.allocated_bytes() + owned > budget {
                    self.matches.shrink_to_fit();
                    self.truncated = true;
                    break;
                }
            }
            let mut term_indices = Vec::with_capacity(hit.terms.count_ones() as usize);
            for term in 0..MAX_TERMS {
                if hit.terms & (1 << term) != 0 {
                    term_indices.push(term);
                }
            }
            self.matches.push(ScoutMatch {
                path: file.path.clone(),
                line_number: hit.line + 1,
                term_indices,
            });
            self.owned_bytes += owned;
        }
    }
}

#[derive(Default)]
struct StructureReferences {
    files: Vec<ScoutFileStructure>,
    owned_bytes: usize,
    truncated: bool,
}

impl StructureReferences {
    fn allocated_bytes(&self) -> usize {
        self.files.capacity() * std::mem::size_of::<ScoutFileStructure>() + self.owned_bytes
    }

    fn add(&mut self, file: &FileHits, budget: usize) {
        if self.truncated {
            return;
        }
        let capacity = if self.files.len() == self.files.capacity() {
            self.files
                .capacity()
                .max(4)
                .saturating_mul(2)
                .min(MAX_STRUCTURE_FILES)
        } else {
            self.files.capacity()
        };
        let base = capacity * std::mem::size_of::<ScoutFileStructure>() + self.owned_bytes;
        if self.files.len() == MAX_STRUCTURE_FILES || base + file.path.len() > budget {
            self.truncated = true;
            return;
        }
        if capacity > self.files.capacity()
            && self
                .files
                .try_reserve_exact(capacity - self.files.len())
                .is_err()
        {
            self.truncated = true;
            return;
        }
        let path = file.path.clone();
        let mut definitions: Vec<syntax::Scope> = Vec::new();
        let mut names = 0;
        let mut omitted = false;
        for scope in &file.syntax.scopes {
            let at = file
                .hits
                .partition_point(|hit| hit.line + 1 < scope.start_line);
            if file
                .hits
                .get(at)
                .is_none_or(|hit| hit.line >= scope.end_line)
            {
                continue;
            }
            let capacity = if definitions.len() == definitions.capacity() {
                definitions.capacity().max(4).saturating_mul(2)
            } else {
                definitions.capacity()
            };
            let name = scope.name.as_ref().map_or(0, String::len);
            if self.allocated_bytes()
                + path.capacity()
                + capacity * std::mem::size_of::<syntax::Scope>()
                + names
                + name
                > budget
            {
                omitted = true;
                break;
            }
            if capacity > definitions.capacity()
                && definitions
                    .try_reserve_exact(capacity - definitions.len())
                    .is_err()
            {
                omitted = true;
                break;
            }
            let copy = scope.clone();
            let actual_name = copy.name.as_ref().map_or(0, String::capacity);
            if self.allocated_bytes()
                + path.capacity()
                + definitions.capacity() * std::mem::size_of::<syntax::Scope>()
                + names
                + actual_name
                > budget
            {
                omitted = true;
                break;
            }
            names += actual_name;
            definitions.push(copy);
        }
        let owned =
            path.capacity() + definitions.capacity() * std::mem::size_of::<syntax::Scope>() + names;
        if self.allocated_bytes() + owned > budget {
            self.truncated = true;
            self.files.shrink_to_fit();
            return;
        }
        self.files.push(ScoutFileStructure {
            path,
            language: file.syntax.language,
            status: file.syntax.status,
            definitions,
            definitions_truncated: omitted,
        });
        self.owned_bytes += owned;
        self.truncated |= omitted;
    }
}

/// Optional syntax work has independent bounds and never consumes the lexical scan's
/// input/retention budget or converts a complete text search into a failed search.
#[derive(Default)]
struct SyntaxBudget {
    files: usize,
    bytes: usize,
    elapsed: Duration,
    retained: usize,
}

impl SyntaxBudget {
    fn analyze(&mut self, path: &str, bytes: &[u8]) -> syntax::Analysis {
        let language = syntax::language_name(path);
        let unavailable = |status| syntax::Analysis {
            language,
            status,
            scopes: Vec::new(),
        };
        if language.is_none() {
            return unavailable(syntax::Status::Unsupported);
        }
        let Ok(source) = std::str::from_utf8(bytes) else {
            return unavailable(syntax::Status::InvalidUtf8);
        };
        if self.files == MAX_STRUCTURE_FILES
            || self.bytes + bytes.len() > MAX_SYNTAX_INPUT
            || self.elapsed >= MAX_SYNTAX_TIME
        {
            return unavailable(syntax::Status::Limited);
        }
        self.files += 1;
        self.bytes += bytes.len();
        let started = Instant::now();
        let mut analysis = syntax::analyze(path, source, MAX_SYNTAX_TIME - self.elapsed);
        self.elapsed += started.elapsed();
        if analysis.allocated_bytes() > MAX_SYNTAX_RETAINED - self.retained {
            analysis.scopes = Vec::new();
            analysis.status = syntax::Status::Limited;
        }
        self.retained += analysis.allocated_bytes();
        analysis
    }
}

/// A line that contains at least one term.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Hit {
    /// Index of the line in the file, from zero.
    line: usize,
    /// One bit per term found on the line.
    terms: u64,
}

/// A file that mentions at least one term.
struct FileHits {
    path: String,
    content: Vec<String>,
    hits: Vec<Hit>,
    syntax: syntax::Analysis,
}

/// How much each term, line and file is worth.
///
/// A rare term says more than a common one: `retries.length` on 14 lines outweighs `retries`
/// on 232. A line is worth the rarity of the terms it contains.
struct Scores {
    weights: Vec<f64>,
}

impl Scores {
    fn new(term_count: usize, files: &[FileHits]) -> Scores {
        let mut lines_with = vec![0usize; term_count];
        for hit in files.iter().flat_map(|f| &f.hits) {
            for (term, count) in lines_with.iter_mut().enumerate() {
                if hit.terms & (1 << term) != 0 {
                    *count += 1;
                }
            }
        }
        let weights = lines_with
            .iter()
            .map(|&n| if n == 0 { 0.0 } else { 1.0 / (n as f64).sqrt() })
            .collect();
        Scores { weights }
    }

    fn line(&self, hit: Hit) -> f64 {
        self.weights
            .iter()
            .enumerate()
            .filter(|(term, _)| hit.terms & (1 << term) != 0)
            .map(|(_, weight)| weight)
            .sum()
    }

    /// A file is worth its eight best lines.
    fn file(&self, file: &FileHits) -> f64 {
        let mut lines: Vec<f64> = file.hits.iter().map(|&hit| self.line(hit)).collect();
        lines.sort_by(|a, b| b.partial_cmp(a).unwrap_or(Ordering::Equal));
        lines.iter().take(8).sum()
    }

    /// The `count` most telling lines of a file, in the order of the file.
    fn best_lines(&self, file: &FileHits, count: usize) -> Vec<Hit> {
        let mut best = file.hits.clone();
        best.sort_by(|a, b| {
            self.line(*b)
                .partial_cmp(&self.line(*a))
                .unwrap_or(Ordering::Equal)
                .then(a.line.cmp(&b.line))
        });
        best.truncate(count);
        best.sort();
        best
    }
}

fn is_skipped(path: &str) -> bool {
    if path.split('/').any(|part| SKIPPED_DIRS.contains(&part)) {
        return true;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.ends_with(".min.js") || name.ends_with(".min.css") || name == "package-lock.json" {
        return true;
    }
    name.rsplit_once('.').is_some_and(|(_, extension)| {
        SKIPPED_EXTENSIONS.contains(&extension.to_lowercase().as_str())
    })
}

struct Candidates {
    paths: Vec<String>,
    truncated: bool,
}

/// A bounded Git listing. Command-line configuration takes precedence over repository
/// configuration: even a configured fsmonitor hook must never run during discovery.
/// Use the system Git, not a program selected through the caller's PATH.
fn git_files(dir: &Path, paths: &[String]) -> Result<Option<Vec<u8>>, String> {
    let mut child = match Command::new("/usr/bin/git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "core.preloadIndex=false",
            "-c",
            "core.excludesFile=/dev/null",
            "-C",
        ])
        .arg(dir)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
        ])
        .args(paths)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Ok(None),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take((MAX_LISTING_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < GIT_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => break Err("Git file discovery timed out; restrict paths".to_string()),
            Err(error) => break Err(format!("Git file discovery failed: {error}")),
        }
    };
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let bytes = reader
        .join()
        .map_err(|_| "Git file discovery reader failed".to_string())?
        .map_err(|error| format!("Git file discovery failed: {error}"))?;
    if bytes.len() > MAX_LISTING_BYTES {
        return Err("Git file listing exceeds 2 MiB; restrict paths".to_string());
    }
    let status = status?;
    if status.success() {
        // An empty successful listing is authoritative. Do not walk ignored files instead.
        return Ok(Some(bytes));
    }
    if dir.ancestors().any(|parent| parent.join(".git").exists()) {
        return Err("Git file discovery failed; search not run".to_string());
    }
    Ok(None)
}

/// Tracked and untracked, non-ignored files in a repository; a filtered walk otherwise.
/// Both paths are bounded, and report incomplete enumeration instead of claiming a full map.
fn project_files(dir: &Path, paths: &[String]) -> Result<Candidates, String> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    if let Some(bytes) = git_files(dir, paths)? {
        for name in bytes.split(|b| *b == 0).filter(|name| !name.is_empty()) {
            let name = String::from_utf8_lossy(name).into_owned();
            if seen.contains(&name) {
                continue;
            }
            if files.len() == crate::limits::MAX_FILES {
                return Ok(Candidates {
                    paths: files,
                    truncated: true,
                });
            }
            seen.insert(name.clone());
            files.push(name);
        }
        return Ok(Candidates {
            paths: files,
            truncated: false,
        });
    }
    let roots: Vec<&str> = if paths.is_empty() {
        vec!["."]
    } else {
        paths.iter().map(String::as_str).collect()
    };
    let mut entries = 0;
    for root in roots {
        let walker = walkdir::WalkDir::new(dir.join(root))
            .follow_links(false)
            .max_depth(64)
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !entry.file_type().is_dir()
                    || !SKIPPED_DIRS.contains(&entry.file_name().to_string_lossy().as_ref())
            });
        for entry in walker {
            entries += 1;
            if entries > MAX_WALK_ENTRIES {
                return Ok(Candidates {
                    paths: files,
                    truncated: true,
                });
            }
            let entry = entry.map_err(|error| format!("file discovery failed: {error}"))?;
            if entry.file_type().is_dir() && entry.depth() == 64 {
                return Ok(Candidates {
                    paths: files,
                    truncated: true,
                });
            }
            if !entry.file_type().is_file() {
                continue;
            }
            if let Ok(relative) = entry.path().strip_prefix(dir) {
                let relative = relative.to_string_lossy();
                let relative = relative.trim_start_matches("./");
                if seen.contains(relative) {
                    continue;
                }
                if files.len() == crate::limits::MAX_FILES {
                    return Ok(Candidates {
                        paths: files,
                        truncated: true,
                    });
                }
                seen.insert(relative.to_string());
                files.push(relative.to_string());
            }
        }
    }
    Ok(Candidates {
        paths: files,
        truncated: false,
    })
}

enum FileSearch {
    /// A documented exclusion, such as generated content, binary data or a large file.
    Skipped,
    /// A text file was read and searched, with or without a match.
    Read(Option<FileHits>),
    /// This candidate could not be read; it is not evidence that the terms are absent.
    Unreadable,
}

/// Reads one candidate. Resource exhaustion, expected exclusions and I/O failures remain
/// distinct so that an unreadable file can never become a successful negative result.
fn search_file(
    dir: &Path,
    path: &str,
    terms: &RegexSet,
    remaining_input: &mut usize,
    syntax_budget: &mut SyntaxBudget,
) -> Result<FileSearch, ()> {
    if is_skipped(path) {
        return Ok(FileSearch::Skipped);
    }
    let absolute = dir.join(path);
    let Ok(meta) = std::fs::metadata(&absolute) else {
        return Ok(FileSearch::Unreadable);
    };
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Ok(FileSearch::Skipped);
    }
    if meta.len() > *remaining_input as u64 {
        return Err(());
    }
    let Ok(bytes) = crate::limits::read_regular_file(&absolute) else {
        return Ok(FileSearch::Unreadable);
    };
    if bytes.len() > *remaining_input {
        return Err(());
    }
    *remaining_input -= bytes.len();
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Ok(FileSearch::Skipped);
    }
    let is_binary = bytes[..bytes.len().min(8192)].contains(&0);
    if is_binary {
        return Ok(FileSearch::Skipped);
    }
    if !terms.is_match(&bytes) {
        return Ok(FileSearch::Read(None));
    }
    let content: Vec<String> = String::from_utf8_lossy(&bytes)
        .lines()
        .map(String::from)
        .collect();
    let hits: Vec<Hit> = content
        .iter()
        .enumerate()
        .filter_map(|(line, text)| {
            let found = terms
                .matches(text.as_bytes())
                .iter()
                .fold(0u64, |bits, term| bits | (1 << term));
            (found != 0).then_some(Hit { line, terms: found })
        })
        .collect();
    if hits.is_empty() {
        return Ok(FileSearch::Read(None));
    }
    Ok(FileSearch::Read(Some(FileHits {
        path: path.to_string(),
        content,
        hits,
        syntax: syntax_budget.analyze(path, &bytes),
    })))
}

/// Charge allocations as well as text: many very short lines cost much more than their
/// raw byte count once each line and each hit has its own entry.
fn retained_bytes(file: &FileHits) -> usize {
    file.path.capacity()
        + file.content.capacity() * std::mem::size_of::<String>()
        + file.content.iter().map(String::capacity).sum::<usize>()
        + file.hits.capacity() * std::mem::size_of::<Hit>()
}

/// The last step is a strict UTF-8 byte cap, including the truncation notice. This also
/// covers headers and the first detailed section, which cannot be assumed to fit.
fn bounded_answer(mut answer: String, budget: usize) -> String {
    let budget = budget.min(MAX_ANSWER_BYTES);
    if answer.len() <= budget {
        return answer;
    }
    const NOTICE: &str = "\n[output truncated; narrow terms or paths]\n";
    if budget < NOTICE.len() {
        return "[truncated]".chars().take(budget).collect();
    }
    let mut end = budget - NOTICE.len();
    while !answer.is_char_boundary(end) {
        end -= 1;
    }
    answer.truncate(end);
    answer.push_str(NOTICE);
    answer
}

fn directory_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

/// Orders the files from the most to the least telling.
///
/// Neighbours count: a file that sits next to a strongly matching file is likely part of the
/// same feature, even when it only mentions a common term (`schema.ts` next to `retry.ts`).
/// Each file therefore receives a share of the best score of its directory.
fn rank(files: Vec<FileHits>, scores: &Scores) -> Vec<FileHits> {
    let own: Vec<f64> = files.iter().map(|file| scores.file(file)).collect();
    let mut best_in_directory: HashMap<&str, f64> = HashMap::new();
    for (file, &score) in files.iter().zip(&own) {
        let best = best_in_directory
            .entry(directory_of(&file.path))
            .or_insert(0.0);
        if score > *best {
            *best = score;
        }
    }
    let mut order: Vec<(f64, usize)> = files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let neighbours = best_in_directory[directory_of(&file.path)];
            (own[index] + NEIGHBOUR_SHARE * neighbours, index)
        })
        .collect();
    order.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(Ordering::Equal)
            .then_with(|| files[a.1].path.cmp(&files[b.1].path))
    });
    let mut slots: Vec<Option<FileHits>> = files.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|(_, index)| slots[index].take())
        .collect()
}

fn indent_of(line: &str) -> Option<usize> {
    let mut width = 0;
    for c in line.chars() {
        match c {
            ' ' => width += 1,
            '\t' => width += 4 - (width % 4),
            _ => return Some(width),
        }
    }
    None
}

/// The line that opens the block a line sits in: the closest less-indented line above that
/// looks like a definition, or the closest less-indented line when none does.
fn enclosing(content: &[String], line: usize) -> Option<usize> {
    let mut level = indent_of(&content[line])?;
    let mut closest = None;
    for above in (0..line).rev() {
        if level == 0 {
            break;
        }
        let Some(indent) = indent_of(&content[above]) else {
            continue;
        };
        if indent < level {
            level = indent;
            closest.get_or_insert(above);
            if DEFINITION_WORDS.iter().any(|w| content[above].contains(w)) {
                return Some(above);
            }
        }
    }
    closest
}

/// Prefer a window containing a match, rather than silently displaying only the start of
/// a long line. Ellipses and a notice make omissions explicit, including other matches.
fn clipped(line: &str, preview: &Regex) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line.to_string();
    }
    let focus = preview.find(line.as_bytes()).map_or(0, |hit| hit.start());
    let mut start = focus
        .saturating_sub(MAX_LINE_BYTES / 3)
        .min(line.len().saturating_sub(MAX_LINE_BYTES));
    while !line.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + MAX_LINE_BYTES).min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let before = if start > 0 { "… " } else { "" };
    let after = if end < line.len() { " …" } else { "" };
    format!(
        "{before}{}{after} [line clipped; read full line]",
        &line[start..end]
    )
}

/// Writes the chosen matching lines of a file: the line that opens their block, the line
/// itself (marked `N:`), and the rest of the statement when it continues on more-indented
/// lines (`retries: z` / `.array(…)` / `.max(2)`).
fn write_lines(
    file: &FileHits,
    chosen: &[Hit],
    continuation: usize,
    preview: &Regex,
    out: &mut String,
) {
    let matching: HashSet<usize> = file.hits.iter().map(|hit| hit.line).collect();
    let mut last_header = None;
    let mut last_written: Option<usize> = None;
    for hit in chosen {
        if last_written.is_some_and(|written| hit.line <= written) {
            continue;
        }
        let scope = file.syntax.enclosing(hit.line + 1);
        let header = scope.map(|scope| scope.start_line - 1).or_else(|| {
            (file.syntax.status != syntax::Status::Parsed)
                .then(|| enclosing(&file.content, hit.line))
                .flatten()
        });
        if let Some(header) = header {
            if last_header != Some(header) && header < hit.line {
                let end = scope
                    .map_or(header, |scope| scope.header_end_line.saturating_sub(1))
                    .min(hit.line - 1)
                    .min(file.content.len() - 1);
                let start = header.max(last_written.map_or(0, |line| line + 1));
                let shown_end = end.min(start.saturating_add(MAX_SIGNATURE_LINES - 1));
                for (line, source) in file
                    .content
                    .iter()
                    .enumerate()
                    .take(shown_end + 1)
                    .skip(start)
                {
                    let mark = if matching.contains(&line) { ':' } else { ' ' };
                    let text = clipped(source, preview);
                    out.push_str(&format!("{:>5}{mark} {text}\n", line + 1));
                }
                if shown_end < end {
                    out.push_str(&format!(
                        "       … [signature abbreviated; read lines {}-{}]\n",
                        header + 1,
                        end + 1
                    ));
                }
            }
            last_header = Some(header);
        }
        let text = clipped(&file.content[hit.line], preview);
        out.push_str(&format!("{:>5}: {}\n", hit.line + 1, text));
        last_written = Some(hit.line);

        let base = indent_of(&file.content[hit.line]).unwrap_or(0);
        for next in (hit.line + 1..file.content.len()).take(continuation) {
            // The statement ends at the first line that is blank or not more indented.
            if indent_of(&file.content[next]).is_none_or(|indent| indent <= base) {
                break;
            }
            let mark = if matching.contains(&next) { ':' } else { ' ' };
            let text = clipped(&file.content[next], preview);
            out.push_str(&format!("{:>5}{} {}\n", next + 1, mark, text));
            last_written = Some(next);
        }
    }
}

fn structure_notice(analysis: &syntax::Analysis) -> String {
    let language = analysis.language.unwrap_or("unknown");
    match analysis.status {
        syntax::Status::Parsed => format!("[structure: Tree-sitter {language}; parsed]\n"),
        syntax::Status::Partial => format!(
            "[structure: Tree-sitter {language}; partial syntax; lexical heuristic where needed]\n"
        ),
        syntax::Status::Limited => format!(
            "[structure: Tree-sitter {language}; limited; lexical heuristic where needed]\n"
        ),
        syntax::Status::Unsupported => {
            "[structure: unsupported language; lexical heuristic]\n".to_string()
        }
        syntax::Status::InvalidUtf8 => {
            "[structure: invalid UTF-8; lexical heuristic]\n".to_string()
        }
    }
}

/// Builds the answer: a summary, the most telling files in detail, a short preview of the
/// next ones, then a plain list, all within the budget.
fn render(
    terms: &[String],
    files: &[FileHits],
    counts: &ScoutCounts,
    scores: &Scores,
    preview: &Regex,
    budget: usize,
) -> String {
    let mut out = format!(
        "SCOUT - {} files mention these terms ({} text files read; {} skipped; {} read errors)\n",
        files.len(),
        counts.read,
        counts.skipped,
        counts.errors,
    );
    for (term, text) in terms.iter().enumerate() {
        let bit = 1u64 << term;
        let lines = files
            .iter()
            .flat_map(|f| &f.hits)
            .filter(|hit| hit.terms & bit != 0)
            .count();
        let in_files = files
            .iter()
            .filter(|f| f.hits.iter().any(|hit| hit.terms & bit != 0))
            .count();
        out.push_str(&format!(
            "  \"{text}\": {lines} lines in {in_files} files\n"
        ));
    }
    out.push_str(
        "Lines marked `N:` match; lines marked `N ` show nearby declaration or statement context.\n\
         This is a text search: code that does the same thing without these words is not listed. Look at every file below before concluding.\n",
    );

    // The most telling files, in detail.
    let mut detailed = 0;
    for file in files {
        let mut section = format!(
            "\n## {} ({} lines, {} matching)\n",
            file.path,
            file.content.len(),
            file.hits.len()
        );
        section.push_str(&structure_notice(&file.syntax));
        let best = scores.best_lines(file, LINES_PER_FILE);
        write_lines(file, &best, CONTINUATION_LINES, preview, &mut section);
        if file.hits.len() > best.len() {
            let shown: HashSet<usize> = best.iter().map(|hit| hit.line).collect();
            let others: Vec<String> = file
                .hits
                .iter()
                .filter(|hit| !shown.contains(&hit.line))
                .take(40)
                .map(|hit| (hit.line + 1).to_string())
                .collect();
            section.push_str(&format!(
                "       … {} more matching lines: {}\n",
                file.hits.len() - best.len(),
                others.join(", ")
            ));
        }
        if detailed > 0 && out.len() + section.len() > budget * 65 / 100 {
            break;
        }
        out.push_str(&section);
        detailed += 1;
    }

    // The next files are not skipped: each one shows its most telling lines.
    let rest = &files[detailed..];
    if rest.is_empty() {
        return out;
    }
    out.push_str(&format!(
        "\n## {} other files, each with its most telling lines\n",
        rest.len()
    ));
    let mut previewed = 0;
    for file in rest {
        let mut entry = format!("# {} ({} matching)\n", file.path, file.hits.len());
        entry.push_str(&structure_notice(&file.syntax));
        let best = scores.best_lines(file, LINES_PER_PREVIEW);
        write_lines(file, &best, CONTINUATION_LINES - 1, preview, &mut entry);
        if out.len() + entry.len() > budget {
            break;
        }
        out.push_str(&entry);
        previewed += 1;
    }

    // Whatever is left is at least named.
    let rest = &rest[previewed..];
    if rest.is_empty() {
        return out;
    }
    out.push_str(&format!(
        "\n## {} more files (path: matching lines)\n",
        rest.len()
    ));
    let mut listed = 0;
    for file in rest {
        let entry = format!("{}: {}\n", file.path, file.hits.len());
        if out.len() + entry.len() > budget * 115 / 100 {
            break;
        }
        out.push_str(&entry);
        listed += 1;
    }
    if listed < rest.len() {
        out.push_str(&format!("… and {} more files\n", rest.len() - listed));
    }
    out
}

/// Maps where the terms of the request live in the project rooted at `dir`.
/// This compatibility view preserves the text of both complete and incomplete results.
/// Clients needing the status independently of the display budget should use `scout_result`.
pub fn scout(dir: &Path, request: &Request) -> String {
    scout_result(dir, request).unwrap_or_else(|text| text)
}

/// Return `Ok` for a complete scan (including a complete negative result), or `Err` for
/// invalid input, discovery failures and incomplete scans. An error retains any partial
/// result text. Both variants respect the requested display budget, even when it is zero.
pub fn scout_result(dir: &Path, request: &Request) -> Result<String, String> {
    let report = scout_report(dir, request);
    if report.status == ScoutStatus::Complete {
        Ok(report.text)
    } else {
        Err(report.text)
    }
}

/// Search eligible text candidates and retain status, counts and direct match references
/// independently of the display budget. A complete negative scan is `Complete`; inability
/// to discover the candidate set is `Error`; read/resource failures during a scan are
/// `Partial`. The reference limit never stops the scan or changes its completion status.
pub fn scout_report(dir: &Path, request: &Request) -> ScoutReport {
    let budget = request.budget.min(MAX_ANSWER_BYTES);
    let invalid = |reason: &str| ScoutReport::error(reason.to_string(), reason.to_string(), budget);
    if request.terms.len() > MAX_TERMS
        || request.terms.iter().any(|term| term.len() > MAX_TERM_BYTES)
        || request.terms.iter().map(String::len).sum::<usize>() > MAX_QUERY_BYTES
    {
        return invalid(
            "scout: too many or oversized terms (maximum 60 terms, 512 bytes each, 16 KiB total)",
        );
    }
    if request.paths.len() > MAX_PATHS
        || request.paths.iter().map(String::len).sum::<usize>() > MAX_QUERY_BYTES
    {
        return invalid("scout: too many or oversized paths (maximum 128 paths, 16 KiB total)");
    }
    let terms: Vec<String> = request
        .terms
        .iter()
        .map(|term| term.trim().to_string())
        .filter(|term| !term.is_empty())
        .collect();
    if terms.is_empty() {
        return invalid("scout: give at least one term");
    }
    let Ok(set) = RegexSetBuilder::new(terms.iter().map(|term| regex::escape(term)))
        .case_insensitive(true)
        .build()
    else {
        return invalid("scout: unreadable terms");
    };
    let pattern = terms
        .iter()
        .map(|term| regex::escape(term))
        .collect::<Vec<_>>()
        .join("|");
    let Ok(preview) = RegexBuilder::new(&pattern).case_insensitive(true).build() else {
        return invalid("scout: unreadable terms");
    };
    let candidates = match project_files(dir, &request.paths) {
        Ok(candidates) => candidates,
        Err(reason) => {
            return ScoutReport::error(
                reason.clone(),
                format!("[search incomplete: {reason}]"),
                budget,
            )
        }
    };
    let mut found = Vec::new();
    let mut counts = ScoutCounts {
        candidates: candidates.paths.len(),
        ..ScoutCounts::default()
    };
    let mut references = MatchReferences::default();
    let mut structure = StructureReferences::default();
    let mut syntax_budget = SyntaxBudget::default();
    let mut failed_paths = Vec::new();
    let mut remaining_input = crate::limits::MAX_INPUT_BYTES;
    let mut retained = 0;
    let mut limited = candidates.truncated;
    for path in &candidates.paths {
        let hit = match search_file(dir, path, &set, &mut remaining_input, &mut syntax_budget) {
            Ok(FileSearch::Read(hit)) => {
                counts.read += 1;
                hit
            }
            Ok(FileSearch::Skipped) => {
                counts.skipped += 1;
                continue;
            }
            Ok(FileSearch::Unreadable) => {
                counts.errors += 1;
                if failed_paths.len() < 5 {
                    let mut shown: String = path.chars().take(200).collect();
                    if shown.len() < path.len() {
                        shown.push('…');
                    }
                    failed_paths.push(shown);
                }
                continue;
            }
            Err(()) => {
                limited = true;
                break;
            }
        };
        if let Some(hit) = hit {
            counts.matching_files += 1;
            counts.matching_lines += hit.hits.len();
            structure.add(
                &hit,
                MAX_METADATA_BYTES.saturating_sub(references.allocated_bytes()),
            );
            references.add(
                &hit,
                MAX_METADATA_BYTES.saturating_sub(structure.allocated_bytes()),
            );
            let bytes = retained_bytes(&hit);
            if bytes > MAX_RETAINED_BYTES - retained {
                limited = true;
                break;
            }
            retained += bytes;
            found.push(hit);
        }
    }
    let mut answer = if found.is_empty() {
        format!(
            "scout: nothing found for {:?} in {} text files read ({} skipped; {} read errors)",
            terms, counts.read, counts.skipped, counts.errors
        )
    } else {
        let scores = Scores::new(terms.len(), &found);
        let ranked = rank(found, &scores);
        render(&terms, &ranked, &counts, &scores, &preview, budget)
    };
    let incomplete = limited || counts.errors > 0;
    let mut reason = None;
    if incomplete {
        // Put the warning before partial results. Status/reason remain independent when
        // the display budget is too small to show even this notice.
        let mut reasons = Vec::new();
        if limited {
            reasons.push("resource limit reached; restrict paths".to_string());
        }
        if counts.errors > 0 {
            reasons.push(format!(
                "{} files could not be read; check file access",
                counts.errors
            ));
        }
        let details = if failed_paths.is_empty() {
            String::new()
        } else {
            format!("Unreadable candidates (up to 5): {:?}\n", failed_paths)
        };
        let explanation = reasons.join("; ");
        answer.insert_str(0, &format!("[search incomplete: {explanation}]\n{details}"));
        reason = Some(explanation);
    }
    let display_truncated = answer.len() > budget;
    ScoutReport {
        text: bounded_answer(answer, budget),
        status: if incomplete {
            ScoutStatus::Partial
        } else {
            ScoutStatus::Complete
        },
        reason,
        terms,
        counts,
        matches: references.matches,
        metadata_truncated: references.truncated,
        display_truncated,
        structure: structure.files,
        structure_truncated: structure.truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kiro-scout-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/x")).unwrap();
        std::fs::write(
            dir.join("src/retry.ts"),
            "import { z } from 'zod'\n\nexport function setRetries(retries: number[]) {\n  if (retries.length > 2) {\n    return refuse('two retries at most')\n  }\n  return retries\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/schema.ts"),
            "export const schema = z.object({\n  retries: z\n    .array(z.number())\n    .max(2),\n})\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/other.ts"), "export const unrelated = 1\n").unwrap();
        std::fs::write(dir.join("node_modules/x/index.js"), "const retries = 3\n").unwrap();
        dir
    }

    fn ask(dir: &Path, terms: &[&str]) -> String {
        let request = Request {
            terms: terms.iter().map(|t| t.to_string()).collect(),
            paths: Vec::new(),
            budget: 4000,
        };
        scout(dir, &request)
    }

    #[test]
    fn shows_matching_lines_inside_their_function() {
        let dir = fixture("map");
        let answer = ask(&dir, &["retries.length", "Retries at most"]);
        assert!(
            answer.contains("## src/retry.ts (8 lines, 2 matching)"),
            "{answer}"
        );
        assert!(
            answer.contains("    3  export function setRetries(retries: number[]) {"),
            "{answer}"
        );
        assert!(
            answer.contains("    4:   if (retries.length > 2) {"),
            "{answer}"
        );
        assert!(
            answer.contains("    5:     return refuse('two retries at most')"),
            "{answer}"
        );
        assert!(!answer.contains("other.ts"));
        assert!(!answer.contains("node_modules"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shows_the_rest_of_a_statement_that_continues() {
        let dir = fixture("continuation");
        let answer = ask(&dir, &["retries"]);
        // The limit is two lines below the only line that holds the term.
        assert!(answer.contains("    2:   retries: z"), "{answer}");
        assert!(answer.contains("    4      .max(2),"), "{answer}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rare_term_outranks_a_common_one() {
        let dir = fixture("rank");
        let answer = ask(&dir, &["retries", "two retries at most"]);
        let retry = answer.find("src/retry.ts").unwrap();
        let schema = answer.find("src/schema.ts").unwrap();
        assert!(retry < schema, "{answer}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn says_so_when_nothing_matches() {
        let dir = fixture("none");
        let answer = ask(&dir, &["zzzz"]);
        assert!(answer.starts_with("scout: nothing found"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
