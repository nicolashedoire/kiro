//! The engine: runs a command line along the shortest path.
//!
//! It receives a command exactly as an AI agent writes it (`sed -n '10,40p' a.ts`,
//! `grep -rn x src | head -5`, `cd src && wc -l a.ts; cat b.ts`) and runs it in-process:
//! reading, searching and counting happen right here, with no shell and no program started.
//!
//! The engine never approximates. A command line is either run entirely inside the engine,
//! or not at all: [`run_native`] returns `Err` with the reason, and [`run`] then hands the
//! whole line, untouched, to the regular shell.

use crate::builtins::{self, StageResult};
use crate::grep::{self, Syntax};
use crate::limits::{self, append, append_error};
use crate::shell::{self, Link, Word};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

/// The state kept from one command to the next.
pub struct Session {
    /// The working directory. `cd` changes it; no shell is needed to remember it.
    pub dir: PathBuf,
}

/// Preserve a shell-provided logical PWD only when it identifies the actual current
/// directory. std::env::current_dir alone resolves symlinks and loses `cd -L` semantics.
pub fn current_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let physical = std::env::current_dir()?;
    if let Some(pwd) = std::env::var_os("PWD") {
        let logical = PathBuf::from(pwd);
        if logical.is_absolute() && !logical.components().any(|c| c == Component::ParentDir) {
            if let (Ok(a), Ok(b)) = (std::fs::metadata(&logical), std::fs::metadata(&physical)) {
                if a.dev() == b.dev() && a.ino() == b.ino() {
                    return Ok(logical);
                }
            }
        }
    }
    Ok(physical)
}

/// The answer to a command line.
pub struct Reply {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub code: i32,
    /// True when everything ran inside the engine, without starting a single program.
    pub native: bool,
    /// Number of programs started to answer.
    pub spawned: u32,
    /// Why the command was handed to the regular shell (empty when it was not).
    pub fallback_reason: String,
}

/// Runs one stage of a pipeline inside the engine.
fn stage(words: &[Word], dir: &Path, stdin: Option<&[u8]>) -> StageResult {
    // `command grep` is grep. Any other wrapper in front is a different program with its
    // own output: it is left to the regular shell.
    let first = words
        .iter()
        .position(|w| w.text != "command")
        .ok_or("empty command")?;
    let program = &words[first].text;
    if program.contains('/') || words[..=first].iter().any(|w| w.glob) {
        return Err("program paths or expansions".to_string());
    }
    let name = program.as_str();
    let rest = &words[first + 1..];
    // A word the shell would expand (`*.ts`, `{a,b}`, `=name`) is left to the shell.
    if rest.iter().any(|w| w.glob) {
        return Err("unquoted glob".to_string());
    }
    let args: Vec<String> = rest.iter().map(|w| w.text.clone()).collect();
    match name {
        "cat" => builtins::cat(&args, dir, stdin),
        "sed" => builtins::sed(&args, dir, stdin),
        "head" => builtins::head(&args, dir, stdin),
        "tail" => builtins::tail(&args, dir, stdin),
        "wc" => builtins::wc(&args, dir, stdin),
        "cut" => builtins::cut(&args, dir, stdin),
        "echo" => builtins::echo(&args),
        "ls" => builtins::ls(&args, dir),
        "grep" | "egrep" | "fgrep" => {
            let syntax = match name {
                "egrep" => Syntax::Extended,
                "fgrep" => Syntax::Fixed,
                _ => Syntax::Basic,
            };
            let options = grep::parse(&args, syntax)?;
            let out = grep::run(&options, dir, stdin)?;
            Ok((out.stdout, out.stderr, out.code))
        }
        other => Err(format!("command {other}")),
    }
}

/// Keep the logical path, including symlink names, as the shells' default `cd -L` does.
pub(crate) fn logical_directory(path: &Path) -> Option<PathBuf> {
    let mut target = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                target.pop();
            }
            Component::CurDir => {}
            part => target.push(part.as_os_str()),
        }
        if !target.is_dir() {
            return None;
        }
    }
    Some(target)
}

/// Runs `cd [dir]`. Returns `None` when the words are not a `cd` command.
fn change_directory(words: &[Word], dir: &mut PathBuf) -> Option<StageResult> {
    if words.first()?.text != "cd" {
        return None;
    }
    if words.len() > 2 || words.iter().any(|w| w.glob) {
        return Some(Err("cd with several arguments".to_string()));
    }
    let home;
    let wanted = match words.get(1) {
        Some(word) => word.text.as_str(),
        None => {
            home = match std::env::var("HOME") {
                Ok(home) => home,
                Err(_) => return Some(Err("cd without HOME".to_string())),
            };
            &home
        }
    };
    if wanted.is_empty() || wanted.starts_with('-') {
        return Some(Err("cd with an option or empty path".to_string()));
    }
    if !Path::new(wanted).is_absolute() && std::env::var_os("CDPATH").is_some_and(|v| !v.is_empty())
    {
        return Some(Err("cd with CDPATH".to_string()));
    }
    let Some(target) = logical_directory(&builtins::resolve(dir, wanted)) else {
        return Some(Err("cd destination cannot be resolved".to_string()));
    };
    *dir = target;
    Some(Ok((Vec::new(), String::new(), 0)))
}

fn pipeline_stages(text: &str) -> Result<Vec<&str>, String> {
    let mut stages: Vec<&str> = Vec::new();
    let mut rest = text;
    loop {
        let (before, after) = shell::split_pipe(rest);
        if before.trim().is_empty() {
            return Err("empty pipeline stage".to_string());
        }
        stages.push(before);
        if after.is_empty() {
            if before.len() != rest.len() {
                return Err("pipe with nothing after it".to_string());
            }
            break;
        }
        rest = after;
    }
    Ok(stages)
}

/// Parse only, without running a command or touching the session. This is also used on
/// conditional branches that will not run: a shell rejects their malformed grammar too.
fn parsed_stage(text: &str) -> Result<(Vec<Word>, bool, bool), String> {
    let mut command = text.trim();
    let (mut merge, mut discard) = (false, false);
    let mut redirections = 0;
    loop {
        if let Some(shorter) = command.strip_suffix("2>&1") {
            if !shorter.ends_with(char::is_whitespace) {
                return Err("redirection without a separate file descriptor".to_string());
            }
            merge = true;
            command = shorter.trim_end();
        } else if let Some(shorter) = command.strip_suffix("2>/dev/null") {
            if !shorter.ends_with(char::is_whitespace) {
                return Err("redirection without a separate file descriptor".to_string());
            }
            discard = true;
            command = shorter.trim_end();
        } else {
            break;
        }
        redirections += 1;
    }
    if redirections > 1 {
        return Err("several redirections".to_string());
    }
    let words = shell::split_words(command).ok_or("shell syntax not handled")?;
    if words.is_empty() {
        return Err("empty command".to_string());
    }
    Ok((words, merge, discard))
}

/// Runs one pipeline (`a | b | c`) or one `cd` inside the engine.
fn pipeline(text: &str, dir: &mut PathBuf) -> StageResult {
    if let Some(words) = shell::split_words(text.trim()) {
        if let Some(result) = change_directory(&words, dir) {
            return result;
        }
    }
    let mut stream: Option<Vec<u8>> = None;
    let mut stderr = String::new();
    let mut code = 0;
    for text in pipeline_stages(text)? {
        let (words, merge, discard) = parsed_stage(text)?;
        let (mut out, errors, status) = stage(&words, dir, stream.as_deref())?;
        if merge {
            if !out.is_empty() && !errors.is_empty() {
                return Err("2>&1 with interleaved output and errors".to_string());
            }
            let mut merged = errors.into_bytes();
            append(&mut merged, &out)?;
            out = merged;
        } else if !discard {
            append_error(&mut stderr, &errors)?;
        }
        if out.len() > limits::MAX_OUTPUT_BYTES {
            return Err("output exceeds 2 MiB".to_string());
        }
        stream = Some(out);
        code = status;
    }
    Ok((stream.unwrap_or_default(), stderr, code))
}

/// Runs a whole command line inside the engine: pipelines linked by `;`, `&&` and `||`.
///
/// This is how an agent sends several actions in one round trip, with the syntax it already
/// knows. `Err(reason)` means the line is not covered: nothing is kept, not even a `cd`.
pub fn run_native(command: &str, session: &mut Session) -> Result<Reply, String> {
    if command.len() > limits::MAX_COMMAND_BYTES {
        return Err("command exceeds 64 KiB".to_string());
    }
    let command = shell::without_comments(command).ok_or("command exceeds 64 KiB")?;
    let items = shell::split_list(&command).ok_or("unsupported or malformed shell syntax")?;
    if items.is_empty() {
        return Err("empty command".to_string());
    }
    for (_, text) in &items {
        for stage in pipeline_stages(text)? {
            parsed_stage(stage)?;
        }
    }
    // Work on a copy of the directory, committed only when every item is covered.
    let mut dir = session.dir.clone();
    let (mut stdout, mut stderr, mut code) = (Vec::new(), String::new(), 0);
    for (link, text) in items {
        let wanted = match link {
            Link::First | Link::Always => true,
            Link::And => code == 0,
            Link::Or => code != 0,
        };
        if !wanted {
            continue;
        }
        let (out, errors, status) = pipeline(text, &mut dir)?;
        append(&mut stdout, &out)?;
        append_error(&mut stderr, &errors)?;
        code = status;
    }
    session.dir = dir;
    Ok(Reply {
        stdout,
        stderr,
        code,
        native: true,
        spawned: 0,
        fallback_reason: String::new(),
    })
}

/// The shell used for what the engine does not cover: `KIRO_SHELL`, then `SHELL`, then `/bin/sh`.
fn fallback_shell() -> String {
    ["KIRO_SHELL", "SHELL"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

/// Hands the command line, untouched, to the regular shell.
fn fallback(command: &str, session: &Session, reason: String) -> Reply {
    let stages = command.matches('|').count() as u32 + 1;
    let output = Command::new(fallback_shell())
        .arg("-c")
        .arg(command)
        .current_dir(&session.dir)
        .env("PWD", &session.dir)
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) => Reply {
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output.status.code().unwrap_or(-1),
            native: false,
            spawned: 1 + stages,
            fallback_reason: reason,
        },
        Err(error) => Reply {
            stdout: Vec::new(),
            stderr: format!("cannot start the command: {error}\n"),
            code: 127,
            native: false,
            spawned: 0,
            fallback_reason: reason,
        },
    }
}

/// Runs a command line: inside the engine when it is covered, through the regular shell
/// otherwise.
pub fn run(command: &str, session: &mut Session) -> Reply {
    match run_native(command, session) {
        Ok(reply) => reply,
        Err(reason) => fallback(command, session, reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kiro-engine-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("g/s")).unwrap();
        std::fs::write(dir.join("g/n.txt"), "one\ntwo\nthree\nfour\nfive\nsix\n").unwrap();
        std::fs::write(dir.join("g/s/f.txt"), "alpha\nbeta gamma\nAlpha end").unwrap();
        dir
    }

    /// Runs a command that the engine must cover; returns (stdout, exit code).
    fn exec(dir: &Path, command: &str) -> (String, i32) {
        let mut session = Session {
            dir: dir.to_path_buf(),
        };
        let reply = run_native(command, &mut session)
            .unwrap_or_else(|reason| panic!("not handled: {reason}"));
        assert!(reply.native);
        assert_eq!(reply.spawned, 0);
        (String::from_utf8(reply.stdout).unwrap(), reply.code)
    }

    #[test]
    fn prints_lines_like_sed() {
        let dir = fixture("sed");
        assert_eq!(exec(&dir, "sed -n '2,3p' g/n.txt").0, "two\nthree\n");
        assert_eq!(exec(&dir, "sed -n '5,$p' g/n.txt").0, "five\nsix\n");
        // A last line without a newline stays without a newline.
        assert_eq!(exec(&dir, "sed -n 3p g/s/f.txt").0, "Alpha end");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn counts_like_wc() {
        let dir = fixture("wc");
        assert_eq!(exec(&dir, "wc -l g/n.txt").0, "       6 g/n.txt\n");
        assert_eq!(
            exec(&dir, "wc -l g/n.txt g/s/f.txt").0,
            "       6 g/n.txt\n       2 g/s/f.txt\n       8 total\n"
        );
        assert_eq!(
            exec(&dir, "wc g/n.txt").0,
            "       6       6      28 g/n.txt\n"
        );
        assert_eq!(exec(&dir, "cat g/n.txt | wc -l").0, "       6\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn head_and_tail() {
        let dir = fixture("head");
        assert_eq!(exec(&dir, "head -2 g/n.txt").0, "one\ntwo\n");
        assert_eq!(exec(&dir, "tail -n 2 g/n.txt").0, "five\nsix\n");
        assert_eq!(exec(&dir, "tail -n +5 g/n.txt").0, "five\nsix\n");
        assert_eq!(
            exec(&dir, "head -n 1 g/n.txt g/s/f.txt").0,
            "==> g/n.txt <==\none\n\n==> g/s/f.txt <==\nalpha\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pipelines_run_without_starting_a_program() {
        let dir = fixture("pipe");
        let (out, code) = exec(&dir, "grep -rn alpha g | head -1");
        assert_eq!(out, "g/s/f.txt:1:alpha\n");
        assert_eq!(code, 0);
        assert_eq!(exec(&dir, "cd g && grep -c e n.txt").0, "3\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_lists_run_in_one_go() {
        let dir = fixture("list");
        let (out, code) = exec(
            &dir,
            "echo '== lines'; wc -l g/n.txt && grep -c e g/n.txt; cat g/absent 2>/dev/null || echo missing",
        );
        assert_eq!(out, "== lines\n       6 g/n.txt\n3\nmissing\n");
        assert_eq!(code, 0);
        // `&&` stops after a failure, and the failure is the result.
        let (out, code) = exec(&dir, "grep zzz g/n.txt && echo found");
        assert_eq!(out, "");
        assert_eq!(code, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_list_with_one_uncovered_item_changes_nothing() {
        let dir = fixture("rollback");
        let mut session = Session { dir: dir.clone() };
        assert!(run_native("cd g && python3 x.py", &mut session).is_err());
        assert_eq!(session.dir, dir, "the directory must not move");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cd_understands_the_home_directory() {
        let home = std::env::var("HOME").unwrap();
        let mut session = Session {
            dir: PathBuf::from("/"),
        };
        let reply = run_native("cd ~", &mut session).unwrap();
        assert_eq!(reply.code, 0);
        assert_eq!(session.dir, PathBuf::from(&home));
    }

    #[test]
    fn cuts_columns_and_fields() {
        let dir = fixture("cut");
        assert_eq!(exec(&dir, "cut -c1-2 g/n.txt | head -3").0, "on\ntw\nth\n");
        assert_eq!(
            exec(&dir, "grep -n a g/s/f.txt | cut -d: -f1").0,
            "1\n2\n3\n"
        );
        assert_eq!(exec(&dir, "echo 'a b c' | cut -d' ' -f2-").0, "b c\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lists_names_like_ls() {
        let dir = fixture("ls");
        std::fs::write(dir.join("g/.hidden"), "").unwrap();
        assert_eq!(exec(&dir, "ls g").0, "n.txt\ns\n");
        assert_eq!(exec(&dir, "ls -A g").0, ".hidden\nn.txt\ns\n");
        assert_eq!(exec(&dir, "ls g/n.txt").0, "g/n.txt\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_gives_the_original_message() {
        let dir = fixture("missing");
        let mut session = Session { dir: dir.clone() };
        let reply = run_native("cat g/absent", &mut session).unwrap();
        assert_eq!(reply.stderr, "cat: g/absent: No such file or directory\n");
        assert_eq!(reply.code, 1);
        let reply = run_native("grep x g/absent", &mut session).unwrap();
        assert_eq!(reply.stderr, "grep: g/absent: No such file or directory\n");
        assert_eq!(reply.code, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gives_up_on_commands_it_does_not_cover() {
        let dir = fixture("refuse");
        let mut session = Session { dir: dir.clone() };
        for command in [
            "ls -la",
            "cat g/n.txt > copy.txt",
            "cat g/*.txt",
            "echo =====",
            "rm g/n.txt",
            "sleep 1 & echo x",
        ] {
            assert!(
                run_native(command, &mut session).is_err(),
                "should not be covered: {command}"
            );
        }
        assert!(dir.join("g/n.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fallback_runs_what_the_engine_refuses() {
        let dir = fixture("fallback");
        let mut session = Session { dir: dir.clone() };
        let reply = run("ls -1 g | sort -r", &mut session);
        assert!(!reply.native);
        assert_eq!(String::from_utf8(reply.stdout).unwrap(), "s\nn.txt\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
