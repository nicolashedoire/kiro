//! `grep`, run inside the engine without starting a program.
//!
//! The goal is to return exactly what the system `grep` returns. Whenever an option or a
//! pattern is not covered, we give up (`Err`) and the engine runs the real program: one more
//! hop is better than a different result.

use crate::limits::{self, ReadError, MAX_FILES, MAX_INPUT_BYTES, MAX_OUTPUT_BYTES};
use regex::bytes::{Regex, RegexBuilder};
use std::cell::Cell;
use std::path::Path;

const MAX_GLOB_WORK: usize = 2_000_000;

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
            if matches!(name, "color" | "colour") {
                match inline.as_deref().unwrap_or("auto") {
                    "auto" | "never" => {}
                    _ => return Err("colored grep output".to_string()),
                }
                i += 1;
                continue;
            }
            if inline.is_some() && !needs_value {
                return Err(format!("unexpected value for --{}", name));
            }
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
    if o.max_count == Some(0) {
        return Err("zero max-count differs between grep implementations".to_string());
    }
    if o.max_count.is_some() && (o.before > 0 || o.after > 0) {
        return Err("-m with context".to_string());
    }
    if o.only_matching && (o.before > 0 || o.after > 0 || o.invert) {
        return Err("-o with context or -v".to_string());
    }
    validate_filename_patterns(&o)?;
    Ok(o)
}

fn validate_filename_patterns(o: &Options) -> Result<(), String> {
    if o.include
        .iter()
        .chain(&o.exclude)
        .chain(&o.exclude_dir)
        .any(|p| {
            p.contains("[!")
                || p.contains("[^")
                || p.contains("[[:")
                || p.contains("[[.")
                || p.contains("[[=")
                || p.ends_with('\\')
        })
    {
        return Err("unsupported filename-pattern class or escape".to_string());
    }
    Ok(())
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
                    '<' | '>' => return Err("directional word boundary".to_string()),
                    n if n.is_ascii_alphabetic() => {
                        return Err("non-POSIX alphabetic escape".to_string())
                    }
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
                    return Err("directional word boundary".to_string());
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
                Some('<') | Some('>') => return Err("directional word boundary".to_string()),
                Some(n) if n.is_ascii_alphabetic() => {
                    return Err("non-POSIX alphabetic escape".to_string())
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
                return Err("directional word boundary".to_string());
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
    if o.ignore_case && o.patterns.iter().any(|pattern| !pattern.is_ascii()) {
        return Err("case folding of a non-ASCII pattern".to_string());
    }
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
    // Rust regex chooses the first successful alternative; POSIX grep chooses the longest
    // one. This cannot affect a yes/no line match, but it changes the substrings printed by
    // -o. Refuse ambiguous patterns rather than return different matches.
    if o.only_matching && (branches.len() != 1 || has_alternation(&branches[0])) {
        return Err("-o with alternatives requires POSIX longest-match semantics".to_string());
    }
    // A word-boundary assertion is exact for literal words, but not for arbitrary patterns
    // such as @ (which grep -w can match between two non-word characters). Keep only this
    // well-defined subset until the full POSIX word predicate is implemented.
    if o.word
        && o.patterns.iter().any(|pattern| {
            pattern.is_empty()
                || !pattern
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    {
        return Err("-w with a pattern other than a literal word".to_string());
    }
    for branch in &branches {
        if has_regex_extension(branch) {
            return Err("non-POSIX regular-expression extension".to_string());
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

/// Whether a translated expression contains an actual alternation, not an escaped pipe
/// or a pipe inside a bracket class.
fn has_alternation(pattern: &str) -> bool {
    let mut escaped = false;
    let mut bracket = false;
    for c in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '[' => bracket = true,
            ']' => bracket = false,
            '|' if !bracket => return true,
            _ => {}
        }
    }
    false
}

/// Reject regex-crate extensions whose interpretation does not match the BSD tool.
fn has_regex_extension(pattern: &str) -> bool {
    let mut chars = pattern.chars().peekable();
    let mut bracket = false;
    let mut previous_quantifier = false;
    while let Some(c) = chars.next() {
        if c == '\\' {
            let _ = chars.next();
            previous_quantifier = false;
            continue;
        }
        if c == '[' {
            bracket = true;
        } else if c == ']' {
            bracket = false;
        }
        if bracket
            && matches!(
                (c, chars.peek()),
                ('&', Some('&')) | ('-', Some('-')) | ('~', Some('~'))
            )
        {
            return true;
        }
        if !bracket {
            if c == '(' && chars.peek() == Some(&'?') {
                return true;
            }
            if matches!(c, '?' | '+' | '*' | '{') && previous_quantifier {
                return true;
            }
        }
        previous_quantifier = !bracket && matches!(c, '*' | '+' | '?' | '}');
    }
    false
}

#[derive(Default)]
struct LimitedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl LimitedOutput {
    fn extend_from_slice(&mut self, bytes: &[u8]) {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_OUTPUT_BYTES {
            self.exceeded = true;
        }
        if !self.exceeded {
            self.bytes.extend_from_slice(bytes);
        }
    }

    fn push(&mut self, byte: u8) {
        self.extend_from_slice(&[byte]);
    }
}

/// A filename pattern is compiled once and matched without recursive backtracking.
/// The retry position always advances through the name, so work is polynomial. Native
/// searches additionally share one work budget across includes, excludes and directories.
#[derive(Debug)]
enum NameToken {
    Star,
    Any,
    Literal(char),
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

struct NamePattern(Vec<NameToken>);

fn name_units(text: &str) -> Vec<char> {
    if crate::shell::locale_is_utf8() {
        text.chars().collect()
    } else {
        text.bytes().map(char::from).collect()
    }
}

impl NamePattern {
    fn new(pattern: &str) -> Self {
        let chars = name_units(pattern);
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '*' => {
                    if !matches!(tokens.last(), Some(NameToken::Star)) {
                        tokens.push(NameToken::Star);
                    }
                    i += 1;
                }
                '?' => {
                    tokens.push(NameToken::Any);
                    i += 1;
                }
                '\\' if i + 1 < chars.len() => {
                    tokens.push(NameToken::Literal(chars[i + 1]));
                    i += 2;
                }
                '[' => {
                    let mut from = i + 1;
                    let negated = matches!(chars.get(from), Some('!') | Some('^'));
                    from += usize::from(negated);
                    let mut end = from + usize::from(chars.get(from) == Some(&']'));
                    while end < chars.len() && chars[end] != ']' {
                        end += 1;
                    }
                    if end == chars.len() {
                        tokens.push(NameToken::Literal('['));
                        i += 1;
                        continue;
                    }
                    let mut ranges = Vec::new();
                    while from < end {
                        if from + 2 < end && chars[from + 1] == '-' {
                            ranges.push((chars[from], chars[from + 2]));
                            from += 3;
                        } else {
                            ranges.push((chars[from], chars[from]));
                            from += 1;
                        }
                    }
                    tokens.push(NameToken::Class { negated, ranges });
                    i = end + 1;
                }
                c => {
                    tokens.push(NameToken::Literal(c));
                    i += 1;
                }
            }
        }
        Self(tokens)
    }

    fn matches(&self, name: &str, work: &Cell<usize>) -> Result<bool, String> {
        if crate::shell::locale_is_utf8()
            && !name.is_ascii()
            && self
                .0
                .iter()
                .any(|token| matches!(token, NameToken::Class { .. }))
        {
            return Err("locale-sensitive filename bracket matching".to_string());
        }
        let name = name_units(name);
        let (mut pattern_at, mut name_at) = (0, 0);
        let mut star: Option<(usize, usize)> = None;
        while name_at < name.len() {
            spend_glob_work(work)?;
            let matched = match self.0.get(pattern_at) {
                Some(NameToken::Star) => {
                    star = Some((pattern_at, name_at));
                    pattern_at += 1;
                    continue;
                }
                Some(NameToken::Any) => true,
                Some(NameToken::Literal(c)) => *c == name[name_at],
                Some(NameToken::Class { negated, ranges }) => {
                    let mut inside = false;
                    for (first, last) in ranges {
                        spend_glob_work(work)?;
                        inside |= *first <= name[name_at] && name[name_at] <= *last;
                    }
                    inside != *negated
                }
                None => false,
            };
            if matched {
                pattern_at += 1;
                name_at += 1;
            } else if let Some((at, consumed)) = star {
                let next = consumed + 1;
                star = Some((at, next));
                pattern_at = at + 1;
                name_at = next;
            } else {
                return Ok(false);
            }
        }
        while matches!(self.0.get(pattern_at), Some(NameToken::Star)) {
            pattern_at += 1;
        }
        Ok(pattern_at == self.0.len())
    }
}

fn spend_glob_work(work: &Cell<usize>) -> Result<(), String> {
    let remaining = work
        .get()
        .checked_sub(1)
        .ok_or("grep filename-pattern work limit exceeded")?;
    work.set(remaining);
    Ok(())
}

fn any_name_matches(
    patterns: &[NamePattern],
    name: &str,
    work: &Cell<usize>,
) -> Result<bool, String> {
    for pattern in patterns {
        if pattern.matches(name, work)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn file_is_selected(
    includes: &[NamePattern],
    excludes: &[NamePattern],
    path: &str,
    work: &Cell<usize>,
) -> Result<bool, String> {
    let name = Path::new(path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    Ok(
        (includes.is_empty() || any_name_matches(includes, &name, work)?)
            && !any_name_matches(excludes, &name, work)?,
    )
}

/// File-name matching in the style of `fnmatch` (`*`, `?`, simple `[...]` classes).
pub fn name_matches(pattern: &str, name: &str) -> bool {
    NamePattern::new(pattern)
        .matches(name, &Cell::new(usize::MAX))
        .unwrap_or(false)
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
    stdout: LimitedOutput,
    stderr: String,
    found: bool,
    failed: bool,
    group_printed: bool,
    done: bool,
    files: usize,
    input_bytes: usize,
    refusal: Option<String>,
}

impl Search<'_> {
    fn prefix(&mut self, name: Option<&str>, line_number: usize, separator: u8) {
        if self.show_name {
            self.stdout
                .extend_from_slice(name.unwrap_or("(standard input)").as_bytes());
            self.stdout.push(separator);
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
        // BSD's word predicates, case folding and bracket collation depend on libc's
        // locale. Rust regex uses Unicode/ASCII tables instead. ASCII text is verified;
        // refuse data where that difference can change a result, including NUL-sensitive
        // POSIX bracket matching. Literal Unicode search remains supported.
        if crate::shell::locale_is_utf8()
            && (!bytes.is_ascii() || bytes.contains(&0))
            && (o.word
                || o.ignore_case
                || (o.syntax != Syntax::Fixed && o.patterns.iter().any(|p| p.contains('['))))
        {
            self.refusal = Some("locale-sensitive matching on non-ASCII or NUL input".to_string());
            self.done = true;
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
                            if self.stdout.exceeded {
                                self.done = true;
                                return;
                            }
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
            if self.stdout.exceeded {
                self.done = true;
                return;
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
            if self.show_name {
                self.stdout
                    .extend_from_slice(format!("{}:{}\n", shown, matched).as_bytes());
            } else {
                self.stdout
                    .extend_from_slice(format!("{}\n", matched).as_bytes());
            }
        }
    }

    fn file(&mut self, shown: &str, path: &Path) -> Result<(), String> {
        if self.files >= MAX_FILES {
            return Err("grep file-count limit exceeded".to_string());
        }
        self.files += 1;
        match limits::read_regular_file(path) {
            Ok(bytes) => {
                self.input_bytes = self.input_bytes.saturating_add(bytes.len());
                if self.input_bytes > MAX_INPUT_BYTES {
                    return Err("grep input-byte limit exceeded".to_string());
                }
                self.search(Some(shown), &bytes);
            }
            Err(ReadError::Io(e)) => self.report(shown, &e),
            Err(ReadError::Refused(reason)) => return Err(reason),
        }
        self.check_output()
    }

    fn check_output(&self) -> Result<(), String> {
        if let Some(reason) = &self.refusal {
            return Err(reason.clone());
        }
        if self.stdout.exceeded || self.stderr.len() > MAX_OUTPUT_BYTES {
            Err("grep output-byte limit exceeded".to_string())
        } else {
            Ok(())
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
            let message = format!("grep: {}: {}\n", shown, reason);
            if self.stderr.len().saturating_add(message.len()) > MAX_OUTPUT_BYTES {
                self.stdout.exceeded = true;
                self.done = true;
            } else {
                self.stderr.push_str(&message);
            }
        }
    }
}

/// Runs the search. `stdin` is the text received from a pipeline (`… | grep pattern`).
pub fn run(o: &Options, dir: &Path, stdin: Option<&[u8]>) -> Result<Output, String> {
    validate_filename_patterns(o)?;
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
        stdout: LimitedOutput::default(),
        stderr: String::new(),
        found: false,
        failed: false,
        group_printed: false,
        done: false,
        files: 0,
        input_bytes: 0,
        refusal: None,
    };

    if paths.is_empty() {
        let Some(bytes) = stdin else {
            return Err("no file and no input".to_string());
        };
        if bytes.len() > MAX_INPUT_BYTES {
            return Err("grep input-byte limit exceeded".to_string());
        }
        s.search(None, bytes);
        s.check_output()?;
    }
    let includes: Vec<_> = o.include.iter().map(|p| NamePattern::new(p)).collect();
    let excludes: Vec<_> = o.exclude.iter().map(|p| NamePattern::new(p)).collect();
    let excluded_dirs: Vec<_> = o.exclude_dir.iter().map(|p| NamePattern::new(p)).collect();
    let glob_work = Cell::new(MAX_GLOB_WORK);
    let glob_failed = Cell::new(false);
    let mut visited = 0usize;
    for path in &paths {
        if s.done {
            break;
        }
        let absolute = dir.join(path);
        let meta = match std::fs::metadata(&absolute) {
            Ok(m) => m,
            Err(err) => {
                if file_is_selected(&includes, &excludes, path, &glob_work)? {
                    s.report(path, &err);
                }
                continue;
            }
        };
        if !meta.is_dir() {
            if !file_is_selected(&includes, &excludes, path, &glob_work)? {
                continue;
            }
            s.file(path, &absolute)?;
            continue;
        }
        if !o.recursive {
            s.failed = true;
            if !o.no_messages {
                limits::append_error(&mut s.stderr, &format!("grep: {}: Is a directory\n", path))?;
            }
            continue;
        }
        // BSD grep differs between macOS releases in how it prints a trailing slash.
        // Determining that by running grep would violate the native path's no-spawn promise.
        // Refuse this spelling; the caller may use `dir` or its regular shell instead.
        if path.ends_with('/') {
            return Err("recursive directory with a trailing slash".to_string());
        }
        let root = path.as_str();
        // No sorting: the system grep visits entries in directory order, and so do we, so that
        // `grep -r … | head` returns the same lines.
        let walker = walkdir::WalkDir::new(&absolute)
            .follow_links(false)
            .into_iter()
            .filter_entry(|x| {
                x.depth() == 0
                    || !x.file_type().is_dir()
                    || match any_name_matches(
                        &excluded_dirs,
                        &x.file_name().to_string_lossy(),
                        &glob_work,
                    ) {
                        Ok(excluded) => !excluded,
                        Err(_) => {
                            glob_failed.set(true);
                            false
                        }
                    }
            });
        for entry in walker {
            if glob_failed.get() {
                return Err("grep filename-pattern work limit exceeded".to_string());
            }
            if s.done {
                break;
            }
            visited += 1;
            if visited > MAX_FILES {
                return Err("grep directory-entry limit exceeded".to_string());
            }
            let Ok(x) = entry else { continue };
            if !x.file_type().is_file() {
                continue;
            }
            let name = x.file_name().to_string_lossy();
            if !includes.is_empty() && !any_name_matches(&includes, &name, &glob_work)? {
                continue;
            }
            if any_name_matches(&excludes, &name, &glob_work)? {
                continue;
            }
            let relative = x
                .path()
                .strip_prefix(&absolute)
                .unwrap_or(x.path())
                .to_string_lossy()
                .into_owned();
            let shown = format!("{}/{}", root, relative);
            s.file(&shown, x.path())?;
        }
        if glob_failed.get() {
            return Err("grep filename-pattern work limit exceeded".to_string());
        }
    }
    s.check_output()?;
    let code = if s.failed && !(o.quiet && s.found) {
        2
    } else if s.found {
        0
    } else {
        1
    };
    Ok(Output {
        stdout: s.stdout.bytes,
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

    #[test]
    fn filename_work_is_bounded_without_recursive_backtracking() {
        let pattern = format!("{}b", "*a".repeat(30));
        assert!(!name_matches(&pattern, &"a".repeat(30)));
        assert!(name_matches(&pattern, &format!("{}b", "a".repeat(30))));
        let error = NamePattern::new("*aaab")
            .matches(&"a".repeat(100), &Cell::new(10))
            .unwrap_err();
        assert!(error.contains("work limit"));
    }

    #[test]
    fn refuses_word_patterns_with_non_word_edges() {
        for arguments in ["-w @", "-w .foo", "-w foo.", "-w a.*b"] {
            let o = parse(&args(arguments), Syntax::Basic).unwrap();
            let error = run(&o, Path::new("."), Some(b"@\na@b\n @ \n"))
                .err()
                .expect("ambiguous -w pattern must be refused");
            assert!(error.contains("literal word"), "{arguments}: {error}");
        }
    }

    #[test]
    fn refuses_only_matching_with_posix_ambiguous_alternatives() {
        for arguments in ["-Eo a|ab", "-o a\\|ab", "-Fo -e a -e ab"] {
            let o = parse(&args(arguments), Syntax::Basic).unwrap();
            let error = run(&o, Path::new("."), Some(b"ab\n"))
                .err()
                .expect("longest-match ambiguity must be refused");
            assert!(error.contains("longest-match"), "{arguments}: {error}");
        }
        // A literal pipe is safe; it must not be mistaken for an alternative.
        assert_eq!(grep("-Fo a|ab", "a|ab\n"), "a|ab\n");
        assert_eq!(grep("-o a|ab", "a|ab\n"), "a|ab\n");
    }

    #[test]
    fn refuses_lazy_and_other_rust_regex_extensions() {
        for arguments in ["-Eo a+?", "-Eo a{1,2}?", "-E (?i)a"] {
            let o = parse(&args(arguments), Syntax::Basic).unwrap();
            assert!(run(&o, Path::new("."), Some(b"aaa\n")).is_err());
        }
        assert_eq!(grep("-Eo a*", "baaaab\n"), "aaaa\n");
    }

    #[test]
    fn color_modes_are_never_silently_ignored() {
        for option in ["--color=always", "--colour=always", "--color=invalid"] {
            assert!(parse(&[option.to_string(), "x".to_string()], Syntax::Basic).is_err());
        }
        for option in ["--color=never", "--color=auto", "--color"] {
            assert_eq!(grep(&format!("{option} x"), "x\n"), "x\n");
        }
        assert!(parse(&args("--count=ignored x"), Syntax::Basic).is_err());
    }

    #[test]
    fn bounded_output_is_refused_instead_of_truncated() {
        let o = parse(&args("x"), Syntax::Basic).unwrap();
        let input = vec![b'x'; MAX_OUTPUT_BYTES + 1];
        let error = run(&o, Path::new("."), Some(&input)).err().unwrap();
        assert!(error.contains("output-byte limit"), "{error}");
    }

    #[test]
    fn refuses_an_oversized_pipeline_input() {
        let o = parse(&args("x"), Syntax::Basic).unwrap();
        let input = vec![b'x'; MAX_INPUT_BYTES + 1];
        let error = run(&o, Path::new("."), Some(&input)).err().unwrap();
        assert!(error.contains("input-byte limit"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_devices_before_reading_them() {
        let o = parse(&args("x /dev/zero"), Syntax::Basic).unwrap();
        let error = run(&o, Path::new("."), None).err().unwrap();
        assert!(error.contains("non-regular"), "{error}");
    }

    #[test]
    fn trailing_slash_is_refused_without_a_probe() {
        let dir = std::env::temp_dir().join(format!("kiro-grep-slash-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/file"), "x\n").unwrap();
        let o = parse(&args("-r x src/"), Syntax::Basic).unwrap();
        let error = run(&o, &dir, None).err().unwrap();
        assert!(error.contains("trailing slash"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
