//! Minimal, cautious reading of a shell command line.
//!
//! The engine does not try to be a full shell. As soon as a command uses a construct it does
//! not handle (chaining, redirection, substitution, variables), parsing returns `None` and the
//! command is handed to the regular shell.

/// True when the process runs in a UTF-8 locale (`LC_ALL`, then `LC_CTYPE`, then `LANG`).
/// Programs such as grep and cut count characters in that case, and bytes otherwise.
pub fn locale_is_utf8() -> bool {
    for name in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() {
                return value.to_uppercase().replace('-', "").contains("UTF8");
            }
        }
    }
    false
}

/// One word of a command line. `glob` is true when the shell would expand it: an unquoted
/// `*`, `?`, `[`, `{`, or a leading `=` (zsh replaces `=name` with the path of a command).
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub glob: bool,
}

/// Splits a simple command into words (single quotes, double quotes, backslash, leading `~`).
pub fn split_words(command: &str) -> Option<Vec<Word>> {
    #[derive(PartialEq)]
    enum State {
        Bare,
        Single,
        Double,
    }
    let mut words = Vec::new();
    let mut current = String::new();
    let mut open = false;
    let mut glob = false;
    let mut state = State::Bare;
    let mut chars = command.chars().peekable();
    let is_expansion = |next: Option<&char>| matches!(next, Some(c) if c.is_ascii_alphanumeric() || matches!(*c, '{' | '(' | '_'));
    while let Some(c) = chars.next() {
        match state {
            State::Single => {
                if c == '\'' {
                    state = State::Bare;
                } else {
                    current.push(c);
                }
            }
            State::Double => match c {
                '"' => state = State::Bare,
                '\\' => match chars.peek() {
                    Some(&n) if matches!(n, '"' | '\\' | '$' | '`') => {
                        current.push(n);
                        chars.next();
                    }
                    _ => current.push('\\'),
                },
                '$' => {
                    if is_expansion(chars.peek()) {
                        return None;
                    }
                    current.push('$');
                }
                '`' => return None,
                _ => current.push(c),
            },
            State::Bare => match c {
                ' ' | '\t' => {
                    if open {
                        words.push(Word {
                            text: std::mem::take(&mut current),
                            glob,
                        });
                        open = false;
                        glob = false;
                    }
                }
                '\n' | ';' | '&' | '|' | '<' | '>' | '`' | '(' | ')' => return None,
                '\'' => {
                    state = State::Single;
                    open = true;
                }
                '"' => {
                    state = State::Double;
                    open = true;
                }
                '\\' => {
                    if let Some(n) = chars.next() {
                        current.push(n);
                        open = true;
                    }
                }
                '$' => {
                    if is_expansion(chars.peek()) {
                        return None;
                    }
                    current.push('$');
                    open = true;
                }
                '*' | '?' | '[' | '{' => {
                    glob = true;
                    current.push(c);
                    open = true;
                }
                '=' if !open && matches!(chars.peek(), Some(n) if !n.is_whitespace()) => {
                    glob = true;
                    current.push(c);
                    open = true;
                }
                '~' if !open && matches!(chars.peek(), None | Some('/') | Some(' ')) => {
                    current.push_str(&std::env::var("HOME").unwrap_or_default());
                    open = true;
                }
                _ => {
                    current.push(c);
                    open = true;
                }
            },
        }
    }
    if state != State::Bare {
        return None;
    }
    if open {
        words.push(Word {
            text: current,
            glob,
        });
    }
    Some(words)
}

/// Splits at the first `|` outside quotes (`||` is not a pipe). Returns (before, after).
pub fn split_pipe(command: &str) -> (&str, &str) {
    let bytes = command.as_bytes();
    let (mut single, mut double) = (false, false);
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            b'|' if !single && !double => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                    i += 1;
                } else {
                    return (&command[..i], &command[i + 1..]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    (command, "")
}

/// How an item of a command list is linked to the one before it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Link {
    /// First item of the list.
    First,
    /// After `;` or a newline: always runs.
    Always,
    /// After `&&`: runs if the previous item succeeded.
    And,
    /// After `||`: runs if the previous item failed.
    Or,
}

/// Splits `a && b ; c || d` into its items, outside quotes. Pipes stay inside an item.
/// Returns `None` for a background job (`a & b`).
pub fn split_list(command: &str) -> Option<Vec<(Link, &str)>> {
    fn push<'a>(
        link: Link,
        from: usize,
        to: usize,
        command: &'a str,
        items: &mut Vec<(Link, &'a str)>,
    ) {
        let text = command[from..to].trim();
        if !text.is_empty() {
            let link = if items.is_empty() { Link::First } else { link };
            items.push((link, text));
        }
    }
    let bytes = command.as_bytes();
    let (mut single, mut double) = (false, false);
    let mut items: Vec<(Link, &str)> = Vec::new();
    let mut link = Link::First;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && !single {
            i += 2;
            continue;
        }
        if b == b'\'' && !double {
            single = !single;
        } else if b == b'"' && !single {
            double = !double;
        } else if !single && !double {
            let next = bytes.get(i + 1).copied();
            let previous = if i > 0 { bytes[i - 1] } else { b' ' };
            match b {
                b';' | b'\n' => {
                    push(link, start, i, command, &mut items);
                    link = Link::Always;
                    start = i + 1;
                }
                b'&' if next == Some(b'&') => {
                    push(link, start, i, command, &mut items);
                    link = Link::And;
                    start = i + 2;
                    i += 1;
                }
                b'|' if next == Some(b'|') => {
                    push(link, start, i, command, &mut items);
                    link = Link::Or;
                    start = i + 2;
                    i += 1;
                }
                // `2>&1` is a redirection, not a background job.
                b'&' if previous == b'>' => {}
                b'&' => return None,
                _ => {}
            }
        }
        i += 1;
    }
    if single || double {
        return None;
    }
    push(link, start, bytes.len(), command, &mut items);
    Some(items)
}

/// Separates a leading `cd DIR && ` from the rest of the command.
pub fn leading_cd(command: &str) -> (Option<String>, &str) {
    let c = command.trim_start();
    if let Some(rest) = c.strip_prefix("cd ") {
        if let Some(i) = rest.find(" && ") {
            let dir = rest[..i]
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            if !dir.contains(' ') && !dir.contains('$') {
                return (Some(dir), rest[i + 4..].trim_start());
            }
        }
    }
    (None, c)
}

/// The command with quoted content replaced by `_`, to look for operators safely.
pub fn outside_quotes(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let (mut single, mut double, mut escaped) = (false, false, false);
    for c in command.chars() {
        if escaped {
            escaped = false;
            out.push('_');
            continue;
        }
        match c {
            '\\' if !single => {
                escaped = true;
                out.push('_');
            }
            '\'' if !double => {
                single = !single;
                out.push(c);
            }
            '"' if !single => {
                double = !double;
                out.push(c);
            }
            _ if single || double => out.push('_'),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(command: &str) -> Option<Vec<String>> {
        split_words(command).map(|w| w.into_iter().map(|x| x.text).collect())
    }

    #[test]
    fn splits_quoted_words() {
        assert_eq!(
            texts(r#"grep -rn "a b" 'c d' e\ f"#).unwrap(),
            vec!["grep", "-rn", "a b", "c d", "e f"]
        );
    }

    #[test]
    fn refuses_chaining_redirection_and_expansion() {
        assert!(split_words("grep a b; rm c").is_none());
        assert!(split_words("grep a b > c").is_none());
        assert!(split_words("grep \"$VAR\" b").is_none());
        assert!(split_words("grep $(pwd) b").is_none());
    }

    #[test]
    fn keeps_end_of_line_dollar() {
        assert_eq!(
            texts(r#"grep "end$" a"#).unwrap(),
            vec!["grep", "end$", "a"]
        );
    }

    #[test]
    fn flags_unquoted_globs_only() {
        let words = split_words("grep x src/*.ts '*.md'").unwrap();
        assert!(words[2].glob);
        assert!(!words[3].glob);
    }

    #[test]
    fn flags_what_the_shell_would_expand() {
        let flagged = |command: &str| split_words(command).unwrap().iter().any(|w| w.glob);
        assert!(flagged("echo ====="));
        assert!(flagged("cat src/{a,b}.ts"));
        assert!(flagged("grep foo src/[ab].ts"));
        assert!(!flagged("echo '====='"));
        assert!(!flagged("grep -n 'a[0-9]{2}' f"));
        assert!(!flagged("grep --include=x a b"));
    }

    #[test]
    fn splits_pipes_outside_quotes() {
        assert_eq!(
            split_pipe("grep 'a|b' f | head -2"),
            ("grep 'a|b' f ", " head -2")
        );
        assert_eq!(split_pipe("a || b"), ("a || b", ""));
    }

    #[test]
    fn splits_command_lists() {
        let items = split_list("cd src && grep 'a;b' f | head -2; echo x || cat y 2>&1").unwrap();
        assert_eq!(
            items,
            vec![
                (Link::First, "cd src"),
                (Link::And, "grep 'a;b' f | head -2"),
                (Link::Always, "echo x"),
                (Link::Or, "cat y 2>&1")
            ]
        );
        assert!(split_list("sleep 5 & echo x").is_none());
        assert_eq!(
            split_list("grep x f").unwrap(),
            vec![(Link::First, "grep x f")]
        );
    }

    #[test]
    fn finds_leading_cd() {
        assert_eq!(
            leading_cd("cd src && grep x f"),
            (Some("src".to_string()), "grep x f")
        );
        assert_eq!(leading_cd("grep x f"), (None, "grep x f"));
    }
}
