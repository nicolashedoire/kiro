//! The commands the engine runs itself, without starting a program.
//!
//! Each one reproduces the behaviour of the system tool for the forms it accepts (output,
//! error messages and exit codes included) and returns `Err(reason)` for anything else, so the
//! engine can hand the command to the regular shell instead of answering differently.

use crate::shell;
use std::path::{Path, PathBuf};

/// What a pipeline stage produces: standard output, standard error and exit code.
/// `Err` means "not covered here": the caller must not guess.
pub(crate) type StageResult = Result<(Vec<u8>, String, i32), String>;

/// Resolves a path against the working directory; `~` is the home directory, as in a shell.
pub(crate) fn resolve(dir: &Path, path: &str) -> PathBuf {
    if path == "~" || path.starts_with("~/") {
        return PathBuf::from(format!(
            "{}{}",
            std::env::var("HOME").unwrap_or_default(),
            &path[1..]
        ));
    }
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        dir.join(p)
    }
}

/// The words the system tools use for an I/O error.
pub(crate) fn error_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "No such file or directory".to_string(),
        std::io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
        _ if e.raw_os_error() == Some(21) => "Is a directory".to_string(),
        _ => e.to_string(),
    }
}

/// Reads a file; on failure, records the message the original program would have printed.
fn read_file(
    program: &str,
    dir: &Path,
    file: &str,
    stderr: &mut String,
    code: &mut i32,
) -> Option<Vec<u8>> {
    match std::fs::read(resolve(dir, file)) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            let infix = if program == "wc" { "open: " } else { "" };
            stderr.push_str(&format!(
                "{}: {}: {}{}\n",
                program,
                file,
                infix,
                error_reason(&e)
            ));
            *code = 1;
            None
        }
    }
}

/// `cat [-n] [FILE...]`.
pub(crate) fn cat(args: &[String], dir: &Path, stdin: Option<&[u8]>) -> StageResult {
    let mut numbered = false;
    let mut files: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            "-n" => numbered = true,
            x if x.starts_with('-') => return Err(format!("cat {}", x)),
            _ => files.push(a),
        }
    }
    let (mut stdout, mut stderr, mut code) = (Vec::new(), String::new(), 0);
    let write = |bytes: &[u8], out: &mut Vec<u8>| {
        if !numbered {
            out.extend_from_slice(bytes);
            return;
        }
        for (i, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
            out.extend_from_slice(format!("{:>6}\t", i + 1).as_bytes());
            out.extend_from_slice(line);
        }
    };
    if files.is_empty() {
        write(stdin.ok_or("cat with no file and no input")?, &mut stdout);
    }
    for f in files {
        if let Some(bytes) = read_file("cat", dir, f, &mut stderr, &mut code) {
            write(&bytes, &mut stdout);
        }
    }
    Ok((stdout, stderr, code))
}

/// A line address of a `sed` script.
#[derive(Clone, Copy)]
enum Address {
    Line(usize),
    Last,
}

fn address(text: &str) -> Option<Address> {
    if text == "$" {
        return Some(Address::Last);
    }
    text.parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .map(Address::Line)
}

/// `sed -n 'A,Bp'`: printing lines by number, nothing else.
pub(crate) fn sed(args: &[String], dir: &Path, stdin: Option<&[u8]>) -> StageResult {
    let mut it = args.iter();
    if it.next().map(String::as_str) != Some("-n") {
        return Err("sed without -n".to_string());
    }
    let mut script = it.next().ok_or("sed without a script")?;
    if script == "-e" {
        script = it.next().ok_or("sed -e without a script")?;
    }
    let files: Vec<&String> = it.collect();
    if files.iter().any(|f| f.starts_with('-')) {
        return Err("sed with other options".to_string());
    }
    let mut ranges: Vec<(Address, Option<Address>)> = Vec::new();
    for piece in script.split([';', '\n']) {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let body = piece
            .strip_suffix('p')
            .ok_or("sed: something other than `p`")?;
        let range = match body.split_once(',') {
            Some((a, b)) => (
                address(a.trim()).ok_or("sed: address")?,
                Some(address(b.trim()).ok_or("sed: address")?),
            ),
            None => (address(body.trim()).ok_or("sed: address")?, None),
        };
        ranges.push(range);
    }
    if ranges.is_empty() {
        return Err("sed: empty script".to_string());
    }
    let (mut stderr, mut code) = (String::new(), 0);
    let mut contents: Vec<Vec<u8>> = Vec::new();
    if files.is_empty() {
        contents.push(stdin.ok_or("sed with no file and no input")?.to_vec());
    }
    for f in &files {
        if let Some(bytes) = read_file("sed", dir, f, &mut stderr, &mut code) {
            contents.push(bytes);
        }
    }
    let lines: Vec<&[u8]> = contents
        .iter()
        .flat_map(|c| c.split_inclusive(|b| *b == b'\n'))
        .collect();
    let total = lines.len();
    let number = |a: Address| match a {
        Address::Line(n) => n,
        Address::Last => total,
    };
    let mut stdout = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let n = i + 1;
        for (start, end) in &ranges {
            let first = number(*start);
            let inside = match end {
                None => n == first,
                Some(e) => n == first || (n > first && n <= number(*e)),
            };
            if inside {
                stdout.extend_from_slice(line);
            }
        }
    }
    Ok((stdout, stderr, code))
}

/// Reads `-N`, `-n N`, `-nN` (and `+N` for tail). Returns (count, from_start, files).
fn count_and_files<'a>(
    program: &str,
    args: &'a [String],
) -> Result<(usize, bool, Vec<&'a String>), String> {
    let mut n = 10usize;
    let mut from_start = false;
    let mut files = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let mut value: Option<&str> = None;
        if a == "-n" {
            i += 1;
            value = Some(args.get(i).ok_or("missing value")?.as_str());
        } else if let Some(v) = a.strip_prefix("-n") {
            value = Some(v);
        } else if a.len() > 1 && a.starts_with('-') && a[1..].chars().all(|c| c.is_ascii_digit()) {
            value = Some(&a[1..]);
        } else if a.starts_with('-') || (program == "tail" && a.starts_with('+')) {
            return Err(format!("{} {}", program, a));
        } else {
            files.push(a);
        }
        if let Some(v) = value {
            let v = v.trim();
            if let Some(rest) = v.strip_prefix('+') {
                if program != "tail" {
                    return Err("head +N".to_string());
                }
                from_start = true;
                n = rest.parse().map_err(|_| "unreadable number")?;
            } else {
                n = v
                    .trim_start_matches('-')
                    .parse()
                    .map_err(|_| "unreadable number")?;
            }
        }
        i += 1;
    }
    Ok((n, from_start, files))
}

/// `head [-N | -n N] [FILE...]`.
pub(crate) fn head(args: &[String], dir: &Path, stdin: Option<&[u8]>) -> StageResult {
    let (n, _, files) = count_and_files("head", args)?;
    let first_lines = |bytes: &[u8]| -> Vec<u8> {
        bytes
            .split_inclusive(|b| *b == b'\n')
            .take(n)
            .flatten()
            .copied()
            .collect()
    };
    let (mut stdout, mut stderr, mut code) = (Vec::new(), String::new(), 0);
    if files.is_empty() {
        return Ok((
            first_lines(stdin.ok_or("head with no file and no input")?),
            stderr,
            code,
        ));
    }
    let several = files.len() > 1;
    let mut first = true;
    for f in files {
        if let Some(bytes) = read_file("head", dir, f, &mut stderr, &mut code) {
            if several {
                if !first {
                    stdout.push(b'\n');
                }
                stdout.extend_from_slice(format!("==> {} <==\n", f).as_bytes());
            }
            stdout.extend(first_lines(&bytes));
            first = false;
        }
    }
    Ok((stdout, stderr, code))
}

/// `tail [-N | -n N | -n +N] [FILE]`.
pub(crate) fn tail(args: &[String], dir: &Path, stdin: Option<&[u8]>) -> StageResult {
    let (n, from_start, files) = count_and_files("tail", args)?;
    if files.len() > 1 {
        return Err("tail on several files".to_string());
    }
    let (mut stderr, mut code) = (String::new(), 0);
    let bytes: Vec<u8> = match files.first() {
        Some(f) => match read_file("tail", dir, f, &mut stderr, &mut code) {
            Some(b) => b,
            None => return Ok((Vec::new(), stderr, code)),
        },
        None => stdin.ok_or("tail with no file and no input")?.to_vec(),
    };
    let lines: Vec<&[u8]> = bytes.split_inclusive(|b| *b == b'\n').collect();
    let start = if from_start {
        n.saturating_sub(1).min(lines.len())
    } else {
        lines.len().saturating_sub(n)
    };
    Ok((lines[start..].concat(), stderr, code))
}

/// `wc [-lwc] [FILE...]`, with the column layout of the system tool.
pub(crate) fn wc(args: &[String], dir: &Path, stdin: Option<&[u8]>) -> StageResult {
    let (mut l, mut w, mut c) = (false, false, false);
    let mut files: Vec<&String> = Vec::new();
    for a in args {
        if a.len() > 1 && a.starts_with('-') {
            for x in a[1..].chars() {
                match x {
                    'l' => l = true,
                    'w' => w = true,
                    'c' => c = true,
                    other => return Err(format!("wc -{}", other)),
                }
            }
        } else {
            files.push(a);
        }
    }
    if !l && !w && !c {
        (l, w, c) = (true, true, true);
    }
    let measure = |b: &[u8]| -> (usize, usize, usize) {
        let lines = b.iter().filter(|x| **x == b'\n').count();
        let words = b
            .split(|x| matches!(x, b' ' | b'\n' | b'\t' | b'\r' | 0x0b | 0x0c))
            .filter(|m| !m.is_empty())
            .count();
        (lines, words, b.len())
    };
    let write = |(a, b, d): (usize, usize, usize), name: Option<&str>, out: &mut Vec<u8>| {
        let mut s = String::new();
        if l {
            s.push_str(&format!(" {:>7}", a));
        }
        if w {
            s.push_str(&format!(" {:>7}", b));
        }
        if c {
            s.push_str(&format!(" {:>7}", d));
        }
        if let Some(n) = name {
            s.push(' ');
            s.push_str(n);
        }
        s.push('\n');
        out.extend_from_slice(s.as_bytes());
    };
    let (mut stdout, mut stderr, mut code) = (Vec::new(), String::new(), 0);
    if files.is_empty() {
        write(
            measure(stdin.ok_or("wc with no file and no input")?),
            None,
            &mut stdout,
        );
        return Ok((stdout, stderr, code));
    }
    let mut total = (0, 0, 0);
    for f in &files {
        if let Some(bytes) = read_file("wc", dir, f, &mut stderr, &mut code) {
            let m = measure(&bytes);
            total = (total.0 + m.0, total.1 + m.1, total.2 + m.2);
            write(m, Some(f), &mut stdout);
        }
    }
    if files.len() > 1 {
        write(total, Some("total"), &mut stdout);
    }
    Ok((stdout, stderr, code))
}

/// `echo WORD...`: only the plain form.
///
/// Options and backslashes are refused: shells disagree on them (`echo -n` prints `-n` in
/// `sh` and suppresses the newline in `zsh`), so only the user's own shell can answer.
pub(crate) fn echo(args: &[String]) -> StageResult {
    if args.iter().any(|w| w.contains('\\')) {
        return Err("echo with a backslash".to_string());
    }
    if args.first().is_some_and(|w| w.starts_with('-')) {
        return Err("echo with an option".to_string());
    }
    let mut out = args.join(" ").into_bytes();
    out.push(b'\n');
    Ok((out, String::new(), 0))
}

/// Parses a `cut` list such as `1-200`, `-5`, `3-`, `1,4-6` into inclusive 1-based ranges.
fn cut_list(list: &str) -> Option<Vec<(usize, usize)>> {
    let mut ranges = Vec::new();
    for part in list.split(',') {
        let range = match part.split_once('-') {
            Some(("", end)) => (1, end.parse().ok()?),
            Some((start, "")) => (start.parse().ok()?, usize::MAX),
            Some((start, end)) => (start.parse().ok()?, end.parse().ok()?),
            None => {
                let n = part.parse().ok()?;
                (n, n)
            }
        };
        if range.0 == 0 || range.1 < range.0 {
            return None;
        }
        ranges.push(range);
    }
    if ranges.is_empty() {
        None
    } else {
        Some(ranges)
    }
}

/// `cut -c LIST`, `cut -b LIST`, `cut -d X -f LIST [-s]`.
pub(crate) fn cut(args: &[String], dir: &Path, stdin: Option<&[u8]>) -> StageResult {
    let (mut chars, mut bytes_list, mut fields): (Option<String>, Option<String>, Option<String>) =
        (None, None, None);
    let mut delimiter = "\t".to_string();
    let mut only_delimited = false;
    let mut files: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.len() > 1 && a.starts_with('-') {
            let flag = a.as_bytes()[1] as char;
            if flag == 's' && a.len() == 2 {
                only_delimited = true;
                i += 1;
                continue;
            }
            if !matches!(flag, 'c' | 'b' | 'f' | 'd') {
                return Err(format!("cut {}", a));
            }
            let value = if a.len() > 2 {
                a[2..].to_string()
            } else {
                i += 1;
                args.get(i).cloned().ok_or("cut: missing value")?
            };
            match flag {
                'c' => chars = Some(value),
                'b' => bytes_list = Some(value),
                'f' => fields = Some(value),
                _ => delimiter = value,
            }
        } else {
            files.push(a);
        }
        i += 1;
    }
    let by_chars = chars.is_some() && shell::locale_is_utf8();
    let list = chars
        .as_ref()
        .or(bytes_list.as_ref())
        .or(fields.as_ref())
        .ok_or("cut without a list")?;
    let ranges = cut_list(list).ok_or("cut: unreadable list")?;
    let selected = |n: usize| ranges.iter().any(|(a, b)| *a <= n && n <= *b);
    if fields.is_some() && delimiter.chars().count() != 1 {
        return Err("cut: delimiter".to_string());
    }
    let (mut stderr, mut code) = (String::new(), 0);
    let mut input: Vec<u8> = Vec::new();
    if files.is_empty() {
        input.extend_from_slice(stdin.ok_or("cut with no file and no input")?);
    }
    for f in &files {
        if let Some(bytes) = read_file("cut", dir, f, &mut stderr, &mut code) {
            input.extend(bytes);
        }
    }
    let mut lines: Vec<&[u8]> = input.split(|b| *b == b'\n').collect();
    if input.last() == Some(&b'\n') || input.is_empty() {
        lines.pop();
    }
    // The system tool copies a line that holds no delimiter as it is, so a last line without
    // a newline stays without one. Every other line it writes ends with a newline.
    let unterminated_last = match input.last() {
        Some(b'\n') | None => None,
        Some(_) => Some(lines.len() - 1),
    };
    let mut stdout = Vec::new();
    for (index, line) in lines.into_iter().enumerate() {
        if fields.is_some() {
            let text = std::str::from_utf8(line).map_err(|_| "cut: line is not UTF-8")?;
            if !text.contains(delimiter.as_str()) {
                if !only_delimited {
                    stdout.extend_from_slice(line);
                    if unterminated_last != Some(index) {
                        stdout.push(b'\n');
                    }
                }
                continue;
            }
            let kept: Vec<&str> = text
                .split(delimiter.as_str())
                .enumerate()
                .filter(|(k, _)| selected(k + 1))
                .map(|(_, x)| x)
                .collect();
            stdout.extend_from_slice(kept.join(delimiter.as_str()).as_bytes());
        } else if by_chars {
            let text = std::str::from_utf8(line).map_err(|_| "cut: line is not UTF-8")?;
            let kept: String = text
                .chars()
                .enumerate()
                .filter(|(k, _)| selected(k + 1))
                .map(|(_, c)| c)
                .collect();
            stdout.extend_from_slice(kept.as_bytes());
        } else {
            stdout.extend(
                line.iter()
                    .enumerate()
                    .filter(|(k, _)| selected(k + 1))
                    .map(|(_, b)| *b),
            );
        }
        stdout.push(b'\n');
    }
    Ok((stdout, stderr, code))
}

/// `ls`: plain names only (`-1`, `-a`, `-A`). The long format is left to the real program.
pub(crate) fn ls(args: &[String], dir: &Path) -> StageResult {
    let (mut all, mut almost_all) = (false, false);
    let mut operands: Vec<&String> = Vec::new();
    for a in args {
        if a.len() > 1 && a.starts_with('-') {
            for c in a[1..].chars() {
                match c {
                    '1' => {}
                    'a' => all = true,
                    'A' => almost_all = true,
                    other => return Err(format!("ls -{}", other)),
                }
            }
        } else {
            operands.push(a);
        }
    }
    if operands.len() > 1 {
        return Err("ls with several operands".to_string());
    }
    let shown = operands.first().map(|s| s.as_str()).unwrap_or(".");
    let path = resolve(dir, shown);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            return Ok((
                Vec::new(),
                format!("ls: {}: {}\n", shown, error_reason(&e)),
                1,
            ))
        }
    };
    if !meta.is_dir() {
        return Ok((format!("{}\n", shown).into_bytes(), String::new(), 0));
    }
    let entries = match std::fs::read_dir(&path) {
        Ok(e) => e,
        Err(e) => {
            return Ok((
                Vec::new(),
                format!("ls: {}: {}\n", shown, error_reason(&e)),
                1,
            ))
        }
    };
    let mut names: Vec<Vec<u8>> = Vec::new();
    if all {
        names.push(b".".to_vec());
        names.push(b"..".to_vec());
    }
    for entry in entries.flatten() {
        use std::os::unix::ffi::OsStrExt;
        let name = entry.file_name().as_bytes().to_vec();
        if name.first() == Some(&b'.') && !all && !almost_all {
            continue;
        }
        names.push(name);
    }
    names.sort();
    let mut stdout = Vec::new();
    for name in names {
        stdout.extend(name);
        stdout.push(b'\n');
    }
    Ok((stdout, String::new(), 0))
}
