//! Scout: the map of an area of the code, in one call.
//!
//! An AI agent that starts on a task spends its first ten to twenty round trips rebuilding a
//! picture of the code: grep a word, list a directory, grep another word, read a passage.
//! Each of those round trips re-reads the whole conversation. Scout does that exploration in
//! one go, with no model involved: the agent gives the words it cares about, and gets back
//! the files that mention them, the matching lines, and the function each line sits in.
//!
//! It is a text search and says so in its answer: code that does the same thing without the
//! words is not listed.

use regex::bytes::{RegexSet, RegexSetBuilder};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Stdio};

/// Files larger than this are skipped: generated or vendored, rarely what an agent wants.
const MAX_FILE_BYTES: u64 = 400_000;
/// Longer lines are clipped in the answer.
const MAX_LINE_CHARS: usize = 200;
/// At most this many terms are searched (one bit each in a `u64`).
const MAX_TERMS: usize = 60;
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
    /// Maximum size of the answer, in characters.
    pub budget: usize,
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

/// The files of the project: what git tracks (plus untracked, non-ignored files) when there
/// is a repository, a filtered directory walk otherwise.
fn project_files(dir: &Path, paths: &[String]) -> Vec<String> {
    let listed = Command::new("git")
        .arg("-C")
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
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    if let Ok(output) = listed {
        if output.status.success() && !output.stdout.is_empty() {
            return output
                .stdout
                .split(|b| *b == 0)
                .filter(|name| !name.is_empty())
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .collect();
        }
    }
    let roots: Vec<&str> = if paths.is_empty() {
        vec!["."]
    } else {
        paths.iter().map(String::as_str).collect()
    };
    let mut files = Vec::new();
    for root in roots {
        let walker = walkdir::WalkDir::new(dir.join(root))
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !entry.file_type().is_dir()
                    || !SKIPPED_DIRS.contains(&entry.file_name().to_string_lossy().as_ref())
            });
        for entry in walker.flatten().filter(|e| e.file_type().is_file()) {
            if let Ok(relative) = entry.path().strip_prefix(dir) {
                let relative = relative.to_string_lossy();
                files.push(relative.trim_start_matches("./").to_string());
            }
        }
    }
    files
}

/// Reads one file and returns its matching lines, or `None` when it has none.
fn search_file(dir: &Path, path: &str, terms: &RegexSet) -> Option<FileHits> {
    if is_skipped(path) {
        return None;
    }
    let absolute = dir.join(path);
    let meta = std::fs::metadata(&absolute).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&absolute).ok()?;
    let is_binary = bytes[..bytes.len().min(8192)].contains(&0);
    if is_binary || !terms.is_match(&bytes) {
        return None;
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
        return None;
    }
    Some(FileHits {
        path: path.to_string(),
        content,
        hits,
    })
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

fn clipped(line: &str) -> &str {
    if line.len() <= MAX_LINE_CHARS {
        return line;
    }
    let mut end = MAX_LINE_CHARS;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// Writes the chosen matching lines of a file: the line that opens their block, the line
/// itself (marked `N:`), and the rest of the statement when it continues on more-indented
/// lines (`retries: z` / `.array(…)` / `.max(2)`).
fn write_lines(file: &FileHits, chosen: &[Hit], continuation: usize, out: &mut String) {
    let matching: HashSet<usize> = file.hits.iter().map(|hit| hit.line).collect();
    let mut last_header = None;
    let mut last_written: Option<usize> = None;
    for hit in chosen {
        if last_written.is_some_and(|written| hit.line <= written) {
            continue;
        }
        if let Some(header) = enclosing(&file.content, hit.line) {
            if last_header != Some(header) && last_written.is_none_or(|written| header > written) {
                let text = clipped(&file.content[header]);
                out.push_str(&format!("{:>5}  {}\n", header + 1, text));
            }
            last_header = Some(header);
        }
        let text = clipped(&file.content[hit.line]);
        out.push_str(&format!("{:>5}: {}\n", hit.line + 1, text));
        last_written = Some(hit.line);

        let base = indent_of(&file.content[hit.line]).unwrap_or(0);
        for next in (hit.line + 1..file.content.len()).take(continuation) {
            // The statement ends at the first line that is blank or not more indented.
            if indent_of(&file.content[next]).is_none_or(|indent| indent <= base) {
                break;
            }
            let mark = if matching.contains(&next) { ':' } else { ' ' };
            let text = clipped(&file.content[next]);
            out.push_str(&format!("{:>5}{} {}\n", next + 1, mark, text));
            last_written = Some(next);
        }
    }
}

/// Builds the answer: a summary, the most telling files in detail, a short preview of the
/// next ones, then a plain list, all within the budget.
fn render(
    terms: &[String],
    files: &[FileHits],
    scanned: usize,
    scores: &Scores,
    budget: usize,
) -> String {
    let mut out = format!(
        "SCOUT - {} files mention these terms ({} files scanned)\n",
        files.len(),
        scanned
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
        "Lines marked `N:` match; lines marked `N ` are the block they sit in or the rest of the statement.\n\
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
        let best = scores.best_lines(file, LINES_PER_FILE);
        write_lines(file, &best, CONTINUATION_LINES, &mut section);
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
        let best = scores.best_lines(file, LINES_PER_PREVIEW);
        write_lines(file, &best, CONTINUATION_LINES - 1, &mut entry);
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
pub fn scout(dir: &Path, request: &Request) -> String {
    let terms: Vec<String> = request
        .terms
        .iter()
        .map(|term| term.trim().to_string())
        .filter(|term| !term.is_empty())
        .take(MAX_TERMS)
        .collect();
    if terms.is_empty() {
        return "scout: give at least one term".to_string();
    }
    let Ok(set) = RegexSetBuilder::new(terms.iter().map(|term| regex::escape(term)))
        .case_insensitive(true)
        .build()
    else {
        return "scout: unreadable terms".to_string();
    };
    let candidates = project_files(dir, &request.paths);
    let found: Vec<FileHits> = candidates
        .iter()
        .filter_map(|path| search_file(dir, path, &set))
        .collect();
    if found.is_empty() {
        return format!(
            "scout: nothing found for {:?} in {} files",
            terms,
            candidates.len()
        );
    }
    let scores = Scores::new(terms.len(), &found);
    let ranked = rank(found, &scores);
    render(&terms, &ranked, candidates.len(), &scores, request.budget)
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
