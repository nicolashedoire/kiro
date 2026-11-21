//! The commands the engine runs itself, without starting a program.
//!
//! Each one reproduces the behaviour of the system tool for the forms it accepts (output,
//! error messages and exit codes included) and returns `Err(reason)` for anything else, so the
//! engine can hand the command to the regular shell instead of answering differently.

use crate::limits::{self, append, append_error, ReadError};
use crate::shell;
use std::path::{Path, PathBuf};

/// What a pipeline stage produces: standard output, standard error and exit code.
/// `Err` means "not covered here": the caller must not guess.
pub(crate) type StageResult = Result<(Vec<u8>, String, i32), String>;

/// Resolves an already parsed path. Only the shell parser may expand an unquoted `~`.
pub(crate) fn resolve(dir: &Path, path: &str) -> PathBuf {
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
    read_bytes: &mut usize,
) -> Result<Option<Vec<u8>>, String> {
    match limits::read_regular_file(&resolve(dir, file)) {
        Ok(bytes) => {
            limits::check_input(read_bytes, bytes.len())?;
            Ok(Some(bytes))
        }
        Err(ReadError::Refused(reason)) => Err(format!("{program}: {file}: {reason}")),
        Err(ReadError::Io(e)) => {
            let infix = if program == "wc" { "open: " } else { "" };
            append_error(
                stderr,
                &format!("{}: {}: {}{}\n", program, file, infix, error_reason(&e)),
            )?;
            *code = 1;
            Ok(None)
        }
    }
}

/// `cat [-n] [FILE...]`.
pub(crate) fn cat(args: &[String], dir: &Path, stdin: Option<&[u8]>) -> StageResult {
    let mut numbered = false;
    let mut files: Vec<&String> = Vec::new();
    let mut options_done = false;
    for a in args {
        if a == "-" {
            return Err("cat reading stdin through `-`".to_string());
        }
        if options_done {
            files.push(a);
        } else {
            match a.as_str() {
                "--" => options_done = true,
                "-n" => numbered = true,
                x if x.starts_with('-') => return Err(format!("cat {}", x)),
                _ => {
                    options_done = true;
                    files.push(a);
                }
            }
        }
    }
    let (mut stdout, mut stderr, mut code) = (Vec::new(), String::new(), 0);
    let mut read_bytes = 0;
    let write = |bytes: &[u8], out: &mut Vec<u8>| -> Result<(), String> {
        if !numbered {
            return append(out, bytes);
        }
        for (i, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
            append(out, format!("{:>6}\t", i + 1).as_bytes())?;
            append(out, line)?;
        }
        Ok(())
    };
    if files.is_empty() {
        write(stdin.ok_or("cat with no file and no input")?, &mut stdout)?;
    }
    for f in files {
        if let Some(bytes) = read_file("cat", dir, f, &mut stderr, &mut code, &mut read_bytes)? {
            write(&bytes, &mut stdout)?;
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
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
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
    let mut read_bytes = 0;
    if files.is_empty() {
        contents.push(stdin.ok_or("sed with no file and no input")?.to_vec());
    }
    for f in &files {
        if let Some(bytes) = read_file("sed", dir, f, &mut stderr, &mut code, &mut read_bytes)? {
            contents.push(bytes);
        }
    }
    let lines = || {
        contents
            .iter()
            .flat_map(|c| c.split_inclusive(|b| *b == b'\n'))
    };
    let total = lines().count();
    let number = |a: Address| match a {
        Address::Line(n) => n,
        Address::Last => total,
    };
    let mut stdout = Vec::new();
    for (i, line) in lines().enumerate() {
        let n = i + 1;
        for (start, end) in &ranges {
            let first = number(*start);
            let inside = match end {
                None => n == first,
                Some(e) => n == first || (n > first && n <= number(*e)),
            };
            if inside {
                append(&mut stdout, line)?;
                // BSD sed treats each file's final partial line as a separate input line.
                // It adds its missing newline when any later input line exists, even if
                // the script does not print that later line. The final global line keeps
                // its original termination, including when the following files are empty.
                if n < total && !line.ends_with(b"\n") {
                    append(&mut stdout, b"\n")?;
                }
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
    let mut options_done = false;
    while i < args.len() {
        let a = &args[i];
        if a == "-" {
            return Err(format!("{program} reading stdin through `-`"));
        }
        if options_done {
            files.push(a);
            i += 1;
            continue;
        }
        if a == "--" {
            options_done = true;
            i += 1;
            continue;
        }
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
            options_done = true;
            files.push(a);
        }
        if let Some(v) = value {
            from_start = program == "tail" && v.starts_with('+');
            let digits = if let Some(rest) = v.strip_prefix('+') {
                rest
            } else if let Some(rest) = v.strip_prefix('-') {
                if program == "head" {
                    return Err("head with a negative line count".to_string());
                }
                rest
            } else {
                v
            };
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err("unreadable line count".to_string());
            }
            n = digits.parse().map_err(|_| "unreadable line count")?;
            if n > i64::MAX as usize || (program == "head" && n == 0) {
                return Err("line count outside the supported range".to_string());
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
    let mut read_bytes = 0;
    for f in files {
        if let Some(bytes) = read_file("head", dir, f, &mut stderr, &mut code, &mut read_bytes)? {
            if several {
                if !first {
                    append(&mut stdout, b"\n")?;
                }
                append(&mut stdout, format!("==> {} <==\n", f).as_bytes())?;
            }
            append(&mut stdout, &first_lines(&bytes))?;
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
    let mut read_bytes = 0;
    let bytes: Vec<u8> = match files.first() {
        Some(f) => match read_file("tail", dir, f, &mut stderr, &mut code, &mut read_bytes)? {
            Some(b) => b,
            None => return Ok((Vec::new(), stderr, code)),
        },
        None => stdin.ok_or("tail with no file and no input")?.to_vec(),
    };
    let line_count = bytes.split_inclusive(|b| *b == b'\n').count();
    let start_line = if from_start {
        n.saturating_sub(1).min(line_count)
    } else {
        line_count.saturating_sub(n)
    };
    let offset: usize = bytes
        .split_inclusive(|b| *b == b'\n')
        .take(start_line)
        .map(<[u8]>::len)
        .sum();
    let mut out = Vec::new();
    append(&mut out, &bytes[offset..])?;
    Ok((out, stderr, code))
}

/// `wc [-lwc] [FILE...]`, with the column layout of the system tool.
pub(crate) fn wc(args: &[String], dir: &Path, stdin: Option<&[u8]>) -> StageResult {
    let (mut l, mut w, mut c) = (false, false, false);
    let mut files: Vec<&String> = Vec::new();
    let mut options_done = false;
    for a in args {
        if a == "-" {
            return Err("wc reading stdin through `-`".to_string());
        }
        if !options_done && a == "--" {
            options_done = true;
        } else if !options_done && a.len() > 1 && a.starts_with('-') {
            for x in a[1..].chars() {
                match x {
                    'l' => l = true,
                    'w' => w = true,
                    'c' => c = true,
                    other => return Err(format!("wc -{}", other)),
                }
            }
        } else {
            options_done = true;
            files.push(a);
        }
    }
    if !l && !w && !c {
        (l, w, c) = (true, true, true);
    }
    let measure = |b: &[u8]| -> Result<(usize, usize, usize), String> {
        // BSD's iswspace classification depends on the active locale. Rust's Unicode
        // whitespace table is not a replacement for it. Lines and bytes are locale-free;
        // word counting stays native only for ASCII input, where the classifications agree.
        if w && !b.is_ascii() {
            return Err("wc word counts on non-ASCII input require the system locale".to_string());
        }
        let lines = b.iter().filter(|x| **x == b'\n').count();
        let words = b
            .split(|x| matches!(x, b' ' | b'\n' | b'\t' | b'\r' | 0x0b | 0x0c))
            .filter(|m| !m.is_empty())
            .count();
        Ok((lines, words, b.len()))
    };
    let write = |(a, b, d): (usize, usize, usize),
                 name: Option<&str>,
                 out: &mut Vec<u8>|
     -> Result<(), String> {
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
        append(out, s.as_bytes())
    };
    let (mut stdout, mut stderr, mut code) = (Vec::new(), String::new(), 0);
    if files.is_empty() {
        write(
            measure(stdin.ok_or("wc with no file and no input")?)?,
            None,
            &mut stdout,
        )?;
        return Ok((stdout, stderr, code));
    }
    let mut total = (0, 0, 0);
    let mut read_bytes = 0;
    for f in &files {
        if let Some(bytes) = read_file("wc", dir, f, &mut stderr, &mut code, &mut read_bytes)? {
            let m = measure(&bytes)?;
            total = (total.0 + m.0, total.1 + m.1, total.2 + m.2);
            write(m, Some(f), &mut stdout)?;
        }
    }
    if files.len() > 1 {
        write(total, Some("total"), &mut stdout)?;
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
fn cut_list(list: &str) -> Option<(Vec<(usize, usize)>, usize)> {
    let number = |text: &str| -> Option<usize> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    };
    let mut ranges = Vec::new();
    for part in list.split(',') {
        let range = match part.split_once('-') {
            Some(("", end)) => (1, number(end)?),
            Some((start, "")) => (number(start)?, usize::MAX),
            Some((start, end)) => (number(start)?, number(end)?),
            None => {
                let n = number(part)?;
                (n, n)
            }
        };
        if range.0 == 0 || range.1 < range.0 {
            return None;
        }
        ranges.push(range);
    }
    if ranges.is_empty() {
        return None;
    }
    // BSD scans through the first open-ended start, or through the last finite
    // position. Preserve that boundary before merging: it controls partial-line EOF.
    let scan_to = ranges
        .iter()
        .filter(|(_, end)| *end == usize::MAX)
        .map(|(start, _)| *start)
        .min()
        .unwrap_or_else(|| ranges.iter().map(|(_, end)| *end).max().unwrap());
    // A cut list denotes a set, regardless of ordering or repetitions. Normalize once,
    // rather than testing every range for every input byte/character/field.
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some(previous) = merged.last_mut() {
            if start <= previous.1.saturating_add(1) {
                previous.1 = previous.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    Some((merged, scan_to))
}

/// `cut -c LIST`, `cut -b LIST`, `cut -d X -f LIST [-s]`.
pub(crate) fn cut(args: &[String], dir: &Path, stdin: Option<&[u8]>) -> StageResult {
    let (mut chars, mut bytes_list, mut fields): (Option<String>, Option<String>, Option<String>) =
        (None, None, None);
    let mut delimiter = "\t".to_string();
    let mut explicit_delimiter = false;
    let mut selection_seen = false;
    let mut only_delimited = false;
    let mut files: Vec<&String> = Vec::new();
    let mut i = 0;
    let mut options_done = false;
    while i < args.len() {
        let a = &args[i];
        if a == "-" {
            return Err("cut reading stdin through `-`".to_string());
        }
        if !options_done && a == "--" {
            options_done = true;
        } else if !options_done && a.len() > 1 && a.starts_with('-') {
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
                'c' | 'b' | 'f' => {
                    if selection_seen {
                        return Err("cut with several selection options".to_string());
                    }
                    selection_seen = true;
                    match flag {
                        'c' => chars = Some(value),
                        'b' => bytes_list = Some(value),
                        _ => fields = Some(value),
                    }
                }
                _ => {
                    explicit_delimiter = true;
                    delimiter = value;
                }
            }
        } else {
            options_done = true;
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
    let (ranges, scan_to) = cut_list(list).ok_or("cut: unreadable list")?;
    if fields.is_none() && (explicit_delimiter || only_delimited) {
        return Err("cut field options without -f".to_string());
    }
    if fields.is_some()
        && (delimiter.chars().count() != 1 || (!shell::locale_is_utf8() && delimiter.len() != 1))
    {
        return Err("cut: delimiter".to_string());
    }
    let (mut stderr, mut code) = (String::new(), 0);
    let mut stdout = Vec::new();
    let mut read_bytes = 0;
    // Every input file is processed independently. Concatenating raw bytes would merge a
    // final partial line with the next file and drop selected columns from that next line.
    let mut write_input = |input: &[u8]| -> Result<(), String> {
        let lines = input
            .split_inclusive(|b| *b == b'\n')
            .map(|line| line.strip_suffix(b"\n").unwrap_or(line));
        // The system tool copies a line that holds no delimiter as it is, so a last line without
        // a newline stays without one. Every other line it writes ends with a newline.
        let unterminated_last = match input.last() {
            Some(b'\n') | None => None,
            Some(_) => Some(input.iter().filter(|b| **b == b'\n').count()),
        };
        for (index, line) in lines.into_iter().enumerate() {
            let mut units = line.len();
            let mut range = 0;
            let mut selected = |n: usize| {
                while range < ranges.len() && ranges[range].1 < n {
                    range += 1;
                }
                range < ranges.len() && ranges[range].0 <= n
            };
            if fields.is_some() {
                let text = std::str::from_utf8(line).map_err(|_| "cut: line is not UTF-8")?;
                if !text.contains(delimiter.as_str()) {
                    if !only_delimited {
                        append(&mut stdout, line)?;
                        if unterminated_last != Some(index) {
                            append(&mut stdout, b"\n")?;
                        }
                    }
                    continue;
                }
                let mut wrote = false;
                for (k, field) in text.split(delimiter.as_str()).enumerate() {
                    if selected(k + 1) {
                        if wrote {
                            append(&mut stdout, delimiter.as_bytes())?;
                        }
                        append(&mut stdout, field.as_bytes())?;
                        wrote = true;
                    }
                }
            } else if by_chars && !line.is_ascii() {
                let text = std::str::from_utf8(line).map_err(|_| "cut: line is not UTF-8")?;
                let mut encoded = [0; 4];
                units = 0;
                for (k, character) in text.chars().enumerate() {
                    units = k + 1;
                    if selected(k + 1) {
                        append(&mut stdout, character.encode_utf8(&mut encoded).as_bytes())?;
                    }
                }
            } else {
                // Byte mode, and character mode for ASCII, copy each normalized span.
                for &(start, end) in &ranges {
                    if start > line.len() {
                        break;
                    }
                    append(&mut stdout, &line[start - 1..end.min(line.len())])?;
                }
            }
            // If BSD cut encounters EOF before reaching its selection boundary, it
            // preserves the absence of a newline on the final partial line.
            if fields.is_some() || unterminated_last != Some(index) || scan_to <= units {
                append(&mut stdout, b"\n")?;
            }
        }
        Ok(())
    };
    if files.is_empty() {
        write_input(stdin.ok_or("cut with no file and no input")?)?;
    }
    for f in &files {
        if let Some(bytes) = read_file("cut", dir, f, &mut stderr, &mut code, &mut read_bytes)? {
            write_input(&bytes)?;
        }
    }
    Ok((stdout, stderr, code))
}

/// `ls`: plain names only (`-1`, `-a`, `-A`). The long format is left to the real program.
pub(crate) fn ls(args: &[String], dir: &Path) -> StageResult {
    let (mut all, mut almost_all) = (false, false);
    let mut operands: Vec<&String> = Vec::new();
    let mut options_done = false;
    for a in args {
        if !options_done && a == "--" {
            options_done = true;
        } else if !options_done && a.len() > 1 && a.starts_with('-') {
            for c in a[1..].chars() {
                match c {
                    '1' => {}
                    'a' => all = true,
                    'A' => almost_all = true,
                    other => return Err(format!("ls -{}", other)),
                }
            }
        } else {
            options_done = true;
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
    for entry in entries {
        use std::os::unix::ffi::OsStrExt;
        let entry = entry.map_err(|e| format!("ls directory-entry error: {}", error_reason(&e)))?;
        let name = entry.file_name().as_bytes().to_vec();
        if name.first() == Some(&b'.') && !all && !almost_all {
            continue;
        }
        names.push(name);
        if names.len() > limits::MAX_FILES {
            return Err("directory exceeds 10000 entries".to_string());
        }
    }
    // LC_COLLATE is independent of LC_CTYPE. A byte sort is guaranteed in the C/POSIX
    // locale; non-ASCII names in other locales require the platform collation tables.
    let collation = ["LC_ALL", "LC_COLLATE", "LANG"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| "C".to_string());
    if !matches!(collation.as_str(), "C" | "POSIX") && names.iter().any(|name| !name.is_ascii()) {
        return Err("ls non-ASCII sorting requires the system collation locale".to_string());
    }
    names.sort();
    let mut stdout = Vec::new();
    for name in names {
        append(&mut stdout, &name)?;
        append(&mut stdout, b"\n")?;
    }
    Ok((stdout, String::new(), 0))
}
