//! `grep`, run inside the engine without starting a program.
//!
//! The goal is to return exactly what the system `grep` returns. Whenever an option or a
//! pattern is not covered, we give up (`Err`) and the engine runs the real program: one more
//! hop is better than a different result.

use regex::bytes::{Regex, RegexBuilder};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Syntax {
    Basic,
    Extended,
    Fixed,
}

#[derive(Debug)]
pub struct Options {
    pub recursive: bool,
    pub line_numbers: bool,
    pub ignore_case: bool,
    pub invert: bool,
    pub word: bool,
    pub whole_line: bool,
    pub count: bool,
    pub files_with_matches: bool,
    pub files_without_match: bool,
    pub only_matching: bool,
    pub quiet: bool,
    pub no_messages: bool,
    pub with_filename: Option<bool>,
    pub skip_binary: bool,
    pub binary_as_text: bool,
    pub before: usize,
    pub after: usize,
    pub max_count: Option<usize>,
    pub syntax: Syntax,
    pub patterns: Vec<String>,
    pub paths: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub exclude_dir: Vec<String>,
}

pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub code: i32,
}

/// Parses `grep` arguments. `Err(reason)`: not covered, hand over to the real program.
pub fn parse(args: &[String], syntax: Syntax) -> Result<Options, String> {
    let mut o = Options {
        recursive: false,
        line_numbers: false,
        ignore_case: false,
        invert: false,
        word: false,
        whole_line: false,
        count: false,
        files_with_matches: false,
        files_without_match: false,
        only_matching: false,
        quiet: false,
        no_messages: false,
        with_filename: None,
        skip_binary: false,
        binary_as_text: false,
        before: 0,
        after: 0,
        max_count: None,
        syntax,
        patterns: Vec::new(),
        paths: Vec::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        exclude_dir: Vec::new(),
    };
    let number = |s: &str| {
        s.parse::<usize>()
            .map_err(|_| format!("unreadable number: {}", s))
    };
    let mut operands: Vec<String> = Vec::new();
    let mut i = 0;
    let mut options_done = false;
    while i < args.len() {
        let a = &args[i];
        if options_done || !a.starts_with('-') || a == "-" {
            if a == "-" {
                return Err("reading stdin through `-`".to_string());
            }
            operands.push(a.clone());
            i += 1;
            continue;
        }
        if a == "--" {
            options_done = true;
            i += 1;
            continue;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let needs_value = matches!(
                name,
                "include"
                    | "exclude"
                    | "exclude-dir"
                    | "regexp"
                    | "max-count"
                    | "context"
                    | "after-context"
                    | "before-context"
            );
            let value = if needs_value {
                match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or_else(|| format!("missing value for --{}", name))?
                    }
                }
            } else {
                String::new()
            };
            match name {
                "recursive" => o.recursive = true,
                "line-number" => o.line_numbers = true,
                "ignore-case" => o.ignore_case = true,
                "invert-match" => o.invert = true,
                "word-regexp" => o.word = true,
                "line-regexp" => o.whole_line = true,
                "count" => o.count = true,
                "files-with-matches" => o.files_with_matches = true,
                "files-without-match" => o.files_without_match = true,
                "only-matching" => o.only_matching = true,
                "quiet" | "silent" => o.quiet = true,
                "no-messages" => o.no_messages = true,
                "with-filename" => o.with_filename = Some(true),
                "no-filename" => o.with_filename = Some(false),
                "extended-regexp" => o.syntax = Syntax::Extended,
                "fixed-strings" => o.syntax = Syntax::Fixed,
                "basic-regexp" => o.syntax = Syntax::Basic,
                "text" => o.binary_as_text = true,
                "color" | "colour" => {}
                "include" => o.include.push(value),
                "exclude" => o.exclude.push(value),
                "exclude-dir" => o.exclude_dir.push(value),
                "regexp" => o.patterns.push(value),
                "max-count" => o.max_count = Some(number(&value)?),
                "context" => {
                    let n = number(&value)?;
                    o.before = n;
                    o.after = n;
                }
                "after-context" => o.after = number(&value)?,
                "before-context" => o.before = number(&value)?,
                other => return Err(format!("option --{}", other)),
            }
            i += 1;
            continue;
        }
        // Short options, possibly clustered: -rn, -A3, -e pattern
        let letters: Vec<char> = a[1..].chars().collect();
        if letters.iter().all(|c| c.is_ascii_digit()) {
            let n = number(&a[1..])?;
            o.before = n;
            o.after = n;
            i += 1;
            continue;
        }
        let mut k = 0;
        while k < letters.len() {
            let c = letters[k];
            match c {
                'r' | 'R' => o.recursive = true,
                'n' => o.line_numbers = true,
                'i' | 'y' => o.ignore_case = true,
                'v' => o.invert = true,
                'w' => o.word = true,
                'x' => o.whole_line = true,
                'c' => o.count = true,
                'l' => o.files_with_matches = true,
                'L' => o.files_without_match = true,
                'o' => o.only_matching = true,
                'q' => o.quiet = true,
                's' => o.no_messages = true,
                'H' => o.with_filename = Some(true),
                'h' => o.with_filename = Some(false),
                'I' => o.skip_binary = true,
                'a' => o.binary_as_text = true,
                'E' => o.syntax = Syntax::Extended,
                'F' => o.syntax = Syntax::Fixed,
                'G' => o.syntax = Syntax::Basic,
                'A' | 'B' | 'C' | 'm' | 'e' => {
                    let rest: String = letters[k + 1..].iter().collect();
                    let value = if rest.is_empty() {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or_else(|| format!("missing value for -{}", c))?
                    } else {
                        rest
                    };
                    match c {
                        'A' => o.after = number(&value)?,
                        'B' => o.before = number(&value)?,
                        'C' => {
                            let n = number(&value)?;
                            o.before = n;
                            o.after = n;
                        }
                        'm' => o.max_count = Some(number(&value)?),
                        _ => o.patterns.push(value),
                    }
                    k = letters.len();
                    continue;
                }
                other => return Err(format!("option -{}", other)),
            }
            k += 1;
        }
        i += 1;
    }
    let mut operands = operands.into_iter();
    if o.patterns.is_empty() {
        o.patterns.push(operands.next().ok_or("no pattern")?);
    }
    o.paths = operands.collect();
    if o.max_count.is_some() && (o.before > 0 || o.after > 0) {
        return Err("-m with context".to_string());
    }
    if o.only_matching && (o.before > 0 || o.after > 0 || o.invert) {
        return Err("-o with context or -v".to_string());
    }
    Ok(o)
}

/// Translates a POSIX basic regular expression into the regex engine's syntax.
fn from_basic(pattern: &str) -> Result<String, String> {
    let c: Vec<char> = pattern.chars().collect();
    let mut s = String::new();
    let mut i = 0;
    let mut branch_start = true;
    // True right after a leading `^`: a `*` there is still a literal star.
    let mut after_caret = false;
    while i < c.len() {
        let star_is_literal = branch_start || after_caret;
        after_caret = false;
        match c[i] {
            '\\' => {
                i += 1;
                let Some(&n) = c.get(i) else {
                    return Err("trailing backslash".to_string());
                };
                match n {
                    '(' => {
                        s.push('(');
                        branch_start = true;
                        i += 1;
                        continue;
                    }
                    ')' => s.push(')'),
                    '|' => {
                        s.push('|');
                        branch_start = true;
                        i += 1;
                        continue;
                    }
                    '{' => s.push('{'),
                    '}' => s.push('}'),
                    '+' => s.push('+'),
                    '?' => s.push('?'),
                    '<' | '>' => s.push_str("\\b"),
                    '1'..='9' => return Err("back-reference".to_string()),
                    other => {
                        s.push('\\');
                        s.push(other);
                    }
                }
            }
            '[' => {
                // A bracket expression is copied verbatim up to its closing `]`.
                let start = i;
                i += 1;
                if c.get(i) == Some(&'^') {
                    i += 1;
                }
                if c.get(i) == Some(&']') {
                    i += 1;
                }
                loop {
                    match c.get(i) {
                        None => return Err("unclosed bracket expression".to_string()),
                        Some(']') => break,
                        Some('[') if matches!(c.get(i + 1), Some(':') | Some('.') | Some('=')) => {
                            let mark = c[i + 1];
                            i += 2;
                            while i + 1 < c.len() && !(c[i] == mark && c[i + 1] == ']') {
                                i += 1;
                            }
                            i += 2;
                        }
                        Some('\\') => return Err("backslash in bracket expression".to_string()),
                        Some('[') => return Err("bracket in bracket expression".to_string()),
                        Some(_) => i += 1,
                    }
                }
                let class: String = c[start..=i].iter().collect();
                if class == "[[:<:]]" || class == "[[:>:]]" {
                    s.push_str("\\b");
                } else {
                    s.push_str(&class);
                }
            }
            '*' if star_is_literal => s.push_str("\\*"),
            '^' if !branch_start => s.push_str("\\^"),
            '^' => {
                s.push('^');
                after_caret = true;
            }
            // `$` anchors only at the very end or before `\)`. Before `\|` the system grep
            // takes it as a literal dollar sign.
            '$' if i + 1 < c.len() && !(c[i + 1] == '\\' && c.get(i + 2) == Some(&')')) => {
                s.push_str("\\$")
            }
            '(' | ')' | '|' | '+' | '?' | '{' | '}' => {
                s.push('\\');
                s.push(c[i]);
            }
            other => s.push(other),
        }
        branch_start = false;
        i += 1;
    }
    Ok(s)
}

fn from_extended(pattern: &str) -> Result<String, String> {
    let c: Vec<char> = pattern.chars().collect();
    let mut s = String::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '\\' {
            match c.get(i + 1) {
                Some('<') | Some('>') => {
                    s.push_str("\\b");
                    i += 2;
                    continue;
                }
                Some('1'..='9') => return Err("back-reference".to_string()),
                Some(n) => {
                    s.push('\\');
                    s.push(*n);
                    i += 2;
                    continue;
                }
                None => return Err("trailing backslash".to_string()),
            }
        }
        if c[i] == '[' {
            let rest: String = c[i..].iter().collect();
            if rest.starts_with("[[:<:]]") || rest.starts_with("[[:>:]]") {
                s.push_str("\\b");
                i += 7;
                continue;
            }
            if let Some(end) = rest[1..].find(']') {
                if rest[..end + 1].contains('\\') {
                    return Err("backslash in bracket expression".to_string());
                }
            }
        }
        s.push(c[i]);
        i += 1;
    }
    Ok(s)
}

/// Builds the regular expression of a search.
///
/// With `whole_file` the expression is meant to be run once over a whole file instead of line
/// by line: `^` and `$` then match at every line boundary.
fn build_regex(o: &Options, whole_file: bool) -> Result<Regex, String> {
    let mut branches: Vec<String> = Vec::new();
    for pattern in &o.patterns {
        for part in pattern.split('\n') {
            branches.push(match o.syntax {
                Syntax::Fixed => regex::escape(part),
                Syntax::Basic => from_basic(part)?,
                Syntax::Extended => from_extended(part)?,
            });
        }
    }
    let mut body = if branches.len() == 1 {
        branches.remove(0)
    } else {
        branches
            .iter()
            .map(|b| format!("(?:{})", b))
            .collect::<Vec<_>>()
            .join("|")
    };
    if o.word {
        body = format!("\\b(?:{})\\b", body);
    }
    if o.whole_line {
        body = format!("^(?:{})$", body);
    }
    // Like the system grep, count characters in a UTF-8 locale and bytes otherwise:
    // `[^']{20,}` does not match the same lines in the two cases.
    RegexBuilder::new(&body)
        .case_insensitive(o.ignore_case)
        .unicode(crate::shell::locale_is_utf8())
        .multi_line(whole_file)
        .build()
        .map_err(|e| format!("pattern not supported: {}", e))
}

/// File-name matching in the style of `fnmatch` (`*`, `?`, simple `[...]` classes).
pub fn name_matches(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|k| go(&p[1..], &n[k..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some('[') => {
                let Some(end) = p.iter().skip(1).position(|c| *c == ']').map(|x| x + 1) else {
                    return !n.is_empty() && n[0] == '[' && go(&p[1..], &n[1..]);
                };
                let Some(&c) = n.first() else { return false };
                let (negated, class) = match p.get(1) {
                    Some('!') | Some('^') => (true, &p[2..end]),
                    _ => (false, &p[1..end]),
                };
                let mut inside = false;
                let mut k = 0;
                while k < class.len() {
                    if k + 2 < class.len() && class[k + 1] == '-' {
                        if class[k] <= c && c <= class[k + 2] {
                            inside = true;
                        }
                        k += 3;
                    } else {
                        if class[k] == c {
                            inside = true;
                        }
                        k += 1;
                    }
                }
                inside != negated && go(&p[end + 1..], &n[1..])
            }
            Some(c) => n.first() == Some(c) && go(&p[1..], &n[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(32_768)].contains(&0)
}

struct Search<'a> {
    o: &'a Options,
    regex: &'a Regex,
    /// The same expression, run once over a whole file to rule it out quickly. In a recursive
    /// search almost every file holds no match: there is no need to split those into lines.
    whole_file: &'a Regex,
    show_name: bool,
    stdout: Vec<u8>,
    stderr: String,
    found: bool,
    failed: bool,
    group_printed: bool,
    done: bool,
}

impl Search<'_> {
    fn prefix(&mut self, name: Option<&str>, line_number: usize, separator: u8) {
        if self.show_name {
            if let Some(n) = name {
                self.stdout.extend_from_slice(n.as_bytes());
                self.stdout.push(separator);
            }
        }
        if self.o.line_numbers {
            self.stdout
                .extend_from_slice(line_number.to_string().as_bytes());
            self.stdout.push(separator);
        }
    }

    fn search(&mut self, name: Option<&str>, bytes: &[u8]) {
        let (o, regex) = (self.o, self.regex);
        let binary = !o.binary_as_text && is_binary(bytes);
        if binary && o.skip_binary {
            return;
        }
        // A match inside a line is also a match in the whole file, so a file where the
        // whole-file expression finds nothing has no matching line.
        let has_no_match = !self.whole_file.is_match(bytes);
        let nothing_to_print = if o.invert {
            false
        } else {
            has_no_match && !o.count && !o.files_without_match
        };
        if nothing_to_print {
            return;
        }
        let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
        if bytes.last() == Some(&b'\n') || bytes.is_empty() {
            lines.pop();
        }
        let names_only = o.files_with_matches || o.files_without_match;
        let mut matched = 0usize;
        let mut last_printed: Option<usize> = None;
        let mut trailing = 0usize;
        for (i, line) in lines.iter().enumerate() {
            let hit = regex.is_match(line) != o.invert;
            if hit {
                matched += 1;
                self.found = true;
                if o.quiet {
                    self.done = true;
                    return;
                }
                if names_only {
                    break;
                }
                if !o.count {
                    if binary {
                        self.stdout.extend_from_slice(
                            format!(
                                "Binary file {} matches\n",
                                name.unwrap_or("(standard input)")
                            )
                            .as_bytes(),
                        );
                        return;
                    }
                    if o.only_matching {
                        for m in regex.find_iter(line) {
                            if m.start() == m.end() {
                                continue;
                            }
                            self.prefix(name, i + 1, b':');
                            self.stdout.extend_from_slice(m.as_bytes());
                            self.stdout.push(b'\n');
                        }
                    } else {
                        // Leading context lines, then the matching line.
                        let from = i
                            .saturating_sub(o.before)
                            .max(last_printed.map_or(0, |d| d + 1));
                        if (o.before > 0 || o.after > 0)
                            && self.group_printed
                            && last_printed.is_none_or(|d| from > d + 1)
                        {
                            self.stdout.extend_from_slice(b"--\n");
                        }
                        for (k, context) in lines.iter().enumerate().take(i).skip(from) {
                            self.prefix(name, k + 1, b'-');
                            self.stdout.extend_from_slice(context);
                            self.stdout.push(b'\n');
                        }
                        self.prefix(name, i + 1, b':');
                        self.stdout.extend_from_slice(line);
                        self.stdout.push(b'\n');
                        last_printed = Some(i);
                        self.group_printed = true;
                        trailing = o.after;
                    }
                }
                if o.max_count.is_some_and(|m| matched >= m) {
                    break;
                }
            } else if trailing > 0 && !o.count && !names_only && !binary {
                self.prefix(name, i + 1, b'-');
                self.stdout.extend_from_slice(line);
                self.stdout.push(b'\n');
                last_printed = Some(i);
                trailing -= 1;
            }
        }
        let shown = name.unwrap_or("(standard input)");
        if o.files_with_matches {
            if matched > 0 {
                self.stdout
                    .extend_from_slice(format!("{}\n", shown).as_bytes());
            }
        } else if o.files_without_match {
            if matched == 0 {
                self.stdout
                    .extend_from_slice(format!("{}\n", shown).as_bytes());
            }
        } else if o.count {
            if self.show_name && name.is_some() {
                self.stdout
                    .extend_from_slice(format!("{}:{}\n", shown, matched).as_bytes());
            } else {
                self.stdout
                    .extend_from_slice(format!("{}\n", matched).as_bytes());
            }
        }
    }

    fn file(&mut self, shown: &str, path: &Path) {
        match std::fs::read(path) {
            Ok(bytes) => self.search(Some(shown), &bytes),
            Err(e) => self.report(shown, &e),
        }
    }

    fn report(&mut self, shown: &str, e: &std::io::Error) {
        self.failed = true;
        if !self.o.no_messages {
            let reason = match e.kind() {
                std::io::ErrorKind::NotFound => "No such file or directory".to_string(),
                std::io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
                _ => e.to_string(),
            };
            self.stderr
                .push_str(&format!("grep: {}: {}\n", shown, reason));
        }
    }
}

/// Whether the system grep keeps the slash of `dir/` in the paths it prints (`dir//file`).
///
/// This differs between macOS versions, so the engine asks the tool itself, once per process:
/// it searches a one-file directory and looks at the path that comes back. `None` when the
/// question cannot be answered; the search is then left to the system tool.
fn system_grep_keeps_trailing_slash() -> Option<bool> {
    static ANSWER: OnceLock<Option<bool>> = OnceLock::new();
    *ANSWER.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("kiro-grep-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(dir.join("f"), "x\n").ok()?;
        let output = Command::new("grep")
            .arg("-r")
            .arg("x")
            .arg(format!("{}/", dir.display()))
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        let printed = String::from_utf8_lossy(&output.ok()?.stdout).into_owned();
        if printed.is_empty() {
            return None;
        }
        Some(printed.contains("//f:"))
    })
}

/// Runs the search. `stdin` is the text received from a pipeline (`… | grep pattern`).
pub fn run(o: &Options, dir: &Path, stdin: Option<&[u8]>) -> Result<Output, String> {
    let regex = build_regex(o, false)?;
    let whole_file = build_regex(o, true)?;
    let paths: Vec<String> = if o.recursive && o.paths.is_empty() {
        vec![".".to_string()]
    } else {
        o.paths.clone()
    };
    let show_name = o.with_filename.unwrap_or(o.recursive || paths.len() > 1);
    let mut s = Search {
        o,
        regex: &regex,
        whole_file: &whole_file,
        show_name,
        stdout: Vec::new(),
        stderr: String::new(),
        found: false,
        failed: false,
        group_printed: false,
        done: false,
    };

    if paths.is_empty() {
        let Some(bytes) = stdin else {
            return Err("no file and no input".to_string());
        };
        s.search(None, bytes);
    }
    for path in &paths {
        if s.done {
            break;
        }
        let absolute = dir.join(path);
        let meta = match std::fs::metadata(&absolute) {
            Ok(m) => m,
            Err(err) => {
                s.report(path, &err);
                continue;
            }
        };
        if !meta.is_dir() {
            s.file(path, &absolute);
            continue;
        }
        if !o.recursive {
            s.failed = true;
            if !o.no_messages {
                s.stderr
                    .push_str(&format!("grep: {}: Is a directory\n", path));
            }
            continue;
        }
        // `grep -r x dir/`: the root is printed as the system grep prints it.
        let root = match path.strip_suffix('/') {
            None => path.as_str(),
            Some(trimmed) if trimmed.ends_with('/') || trimmed.is_empty() => {
                return Err("directory written with several slashes".to_string());
            }
            Some(trimmed) => match system_grep_keeps_trailing_slash() {
                Some(true) => path.as_str(),
                Some(false) => trimmed,
                None => return Err("cannot tell how the system grep prints `dir/`".to_string()),
            },
        };
        let excluded = &o.exclude_dir;
        // No sorting: the system grep visits entries in directory order, and so do we, so that
        // `grep -r … | head` returns the same lines.
        let walker = walkdir::WalkDir::new(&absolute)
            .follow_links(false)
            .into_iter()
            .filter_entry(|x| {
                x.depth() == 0
                    || !x.file_type().is_dir()
                    || !excluded
                        .iter()
                        .any(|m| name_matches(m, &x.file_name().to_string_lossy()))
            });
        for entry in walker {
            if s.done {
                break;
            }
            let Ok(x) = entry else { continue };
            if !x.file_type().is_file() {
                continue;
            }
            let name = x.file_name().to_string_lossy();
            if !o.include.is_empty() && !o.include.iter().any(|m| name_matches(m, &name)) {
                continue;
            }
            if o.exclude.iter().any(|m| name_matches(m, &name)) {
                continue;
            }
            let relative = x
                .path()
                .strip_prefix(&absolute)
                .unwrap_or(x.path())
                .to_string_lossy()
                .into_owned();
            let shown = format!("{}/{}", root, relative);
            s.file(&shown, x.path());
        }
    }
    let code = if s.failed && !(o.quiet && s.found) {
        2
    } else if s.found {
        0
    } else {
        1
    };
    Ok(Output {
        stdout: s.stdout,
        stderr: s.stderr,
        code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split(' ').map(String::from).collect()
    }

    fn grep(arguments: &str, input: &str) -> String {
        let o = parse(&args(arguments), Syntax::Basic).unwrap();
        String::from_utf8(
            run(&o, Path::new("."), Some(input.as_bytes()))
                .unwrap()
                .stdout,
        )
        .unwrap()
    }

    const TEXT: &str = "one\ntwo\nthree\nfour\nfive\nsix\n";

    #[test]
    fn numbers_matching_lines() {
        assert_eq!(grep("-n u", TEXT), "4:four\n");
    }

    #[test]
    fn basic_alternation_works() {
        assert_eq!(grep("-n three\\|six", TEXT), "3:three\n6:six\n");
    }

    #[test]
    fn context_merges_and_separates_like_grep() {
        assert_eq!(
            grep("-n -C1 three\\|six", TEXT),
            "2-two\n3:three\n4-four\n5-five\n6:six\n"
        );
        assert_eq!(
            grep("-n -A1 one\\|five", TEXT),
            "1:one\n2-two\n--\n5:five\n6-six\n"
        );
    }

    #[test]
    fn count_invert_word_and_whole_line() {
        assert_eq!(grep("-c u", TEXT), "1\n");
        assert_eq!(grep("-v u", TEXT), "one\ntwo\nthree\nfive\nsix\n");
        assert_eq!(grep("-w beta", "alphabeta\nbeta gamma\n"), "beta gamma\n");
        assert_eq!(grep("-x alpha", "alpha\nalpha beta\n"), "alpha\n");
    }

    #[test]
    fn only_matching_prints_each_match() {
        assert_eq!(
            grep("-on a", "alpha\nbeta gamma\n"),
            "1:a\n1:a\n2:a\n2:a\n2:a\n"
        );
    }

    #[test]
    fn extended_operators_are_literal_in_basic_syntax() {
        assert_eq!(grep("-n a+b", "a+b\naab\n"), "1:a+b\n");
        assert_eq!(grep("-n f(x)", "f(x)\nfx\n"), "1:f(x)\n");
    }

    #[test]
    fn anchors_follow_the_system_grep_in_basic_syntax() {
        const LINES: &str = "ab\n^ab\nab$\nxab\nabx\n";
        // `^` after `\|` is an anchor
        assert_eq!(grep("x\\|^ab", LINES), "ab\nab$\nxab\nabx\n");
        // `$` before `\|` is a literal dollar sign
        assert_eq!(grep("ab$\\|zz", LINES), "ab$\n");
        // `$` at the very end is an anchor
        assert_eq!(grep("zz\\|ab$", LINES), "ab\n^ab\nxab\n");
        assert_eq!(grep("\\(ab$\\)", LINES), "ab\n^ab\nxab\n");
        // a star right after the leading `^` is a literal star
        assert_eq!(grep("^*ab", LINES), "");
        assert_eq!(grep("^*ab", "*ab\nab\n"), "*ab\n");
    }

    #[test]
    fn gives_up_on_what_is_not_covered() {
        assert!(parse(&args("-P a"), Syntax::Basic).is_err());
        let o = parse(&args("\\(a\\)\\1"), Syntax::Basic).unwrap();
        assert!(run(&o, Path::new("."), Some(b"aa\n")).is_err());
    }

    #[test]
    fn file_name_globs() {
        assert!(name_matches("*.ts", "a.ts"));
        assert!(!name_matches("*.ts", "a.tsx"));
        assert!(name_matches("*.test.*", "a.test.ts"));
        assert!(name_matches("[ab]?.rs", "a1.rs"));
    }
}
