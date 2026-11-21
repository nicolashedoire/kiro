//! A conservative replay benchmark for the native engine and its MCP text transport.
//!
//! Before any shell is started, commands must pass a syntax/program allowlist and the
//! native engine. Unknown options, writing sed scripts, custom executable paths and shell
//! expansions are never replayed. The reference is /bin/sh with a system-only PATH.
//!
//! Run the public, temporary fixture without private sessions or model calls:
//! `cargo run --release --example replay -- --fixture`
//!
//! Recorded sessions remain supported with `--sessions DIR`; use a disposable project
//! checkout, since this runner is a command filter, not an operating-system sandbox.

use kiro::engine::{self, Reply, Session};
use kiro::shell;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const TIME_LIMIT: Duration = Duration::from_secs(8);
const MAX_BYTES: usize = 4_000_000;
const ENGINE_COMMANDS: &[&str] = &[
    "cat", "sed", "head", "tail", "wc", "grep", "egrep", "fgrep", "cut", "ls",
];

struct Run {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: Option<i32>,
    elapsed: Duration,
    truncated: bool,
}

fn read_capped(mut pipe: impl Read) -> (Vec<u8>, bool) {
    let mut buffer = Vec::new();
    let mut block = [0u8; 65536];
    let mut truncated = false;
    loop {
        match pipe.read(&mut block) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let taken = n.min(MAX_BYTES.saturating_sub(buffer.len()));
                buffer.extend_from_slice(&block[..taken]);
                truncated |= taken < n;
            }
        }
    }
    (buffer, truncated)
}

/// The fixed shell avoids user shell startup hooks, aliases and PATH wrappers.
fn run_reference(command: &str, dir: &Path) -> Option<Run> {
    use std::os::unix::process::CommandExt;
    let start = Instant::now();
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .envs(
            ["HOME", "LC_ALL", "LC_CTYPE", "LC_COLLATE", "LANG"]
                .into_iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (name, value))),
        )
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let stderr = child.stderr.take()?;
    let stdout = child.stdout.take()?;
    let error_reader = std::thread::spawn(move || read_capped(stderr));
    let reader = std::thread::spawn(move || read_capped(stdout));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() <= TIME_LIMIT => {
                std::thread::sleep(Duration::from_micros(200));
            }
            _ => {
                // Kill the private process group too: a pipeline must not keep the reader
                // threads alive after its shell has been terminated.
                let _ = Command::new("/bin/kill")
                    .args(["-KILL", "--", &format!("-{}", child.id())])
                    .status();
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let (stdout, out_truncated) = reader.join().ok()?;
    let (stderr, err_truncated) = error_reader.join().ok()?;
    Some(Run {
        stdout,
        stderr,
        code: status?.code(),
        elapsed: start.elapsed(),
        truncated: out_truncated || err_truncated,
    })
}

fn stages_of(command: &str) -> Vec<&str> {
    let mut stages = Vec::new();
    let mut rest = command;
    loop {
        let (before, after) = shell::split_pipe(rest);
        stages.push(before);
        if after.is_empty() {
            break;
        }
        rest = after;
    }
    stages
}

/// Only numeric printing scripts are allowed. `-n` alone does not make sed read-only.
fn printing_sed(args: &[String]) -> bool {
    if args.first().map(String::as_str) != Some("-n") {
        return false;
    }
    let mut at = 1;
    if args.get(at).map(String::as_str) == Some("-e") {
        at += 1;
    }
    let Some(script) = args.get(at) else {
        return false;
    };
    let address = |s: &str| s == "$" || s.parse::<usize>().is_ok_and(|n| n > 0);
    let mut pieces = 0;
    for part in script.split([';', '\n']).map(str::trim) {
        if part.is_empty() {
            continue;
        }
        pieces += 1;
        let Some(range) = part.strip_suffix('p') else {
            return false;
        };
        match range.split_once(',') {
            Some((a, b)) if address(a.trim()) && address(b.trim()) => {}
            None if address(range.trim()) => {}
            _ => return false,
        }
    }
    pieces > 0 && args[at + 1..].iter().all(|s| !s.starts_with('-'))
}

/// Scan every branch, including branches the native preflight might skip.
fn is_safe_and_in_scope(command: &str) -> bool {
    let Some(command) = shell::without_comments(command) else {
        return false;
    };
    let Some(items) = shell::split_list(&command) else {
        return false;
    };
    let mut uses_engine = false;
    for (_, item) in items {
        for stage in stages_of(item) {
            let mut text = stage.trim();
            loop {
                if let Some(prefix) = text.strip_suffix("2>&1") {
                    text = prefix.trim_end();
                } else if let Some(prefix) = text.strip_suffix("2>/dev/null") {
                    text = prefix.trim_end();
                } else {
                    break;
                }
            }
            let Some(words) = shell::split_words(text) else {
                return false;
            };
            if words.iter().any(|w| w.glob) {
                return false;
            }
            let Some(first) = words.iter().position(|w| w.text != "command") else {
                return false;
            };
            let program = words[first].text.as_str();
            if !ENGINE_COMMANDS.contains(&program) && !matches!(program, "cd" | "echo") {
                return false;
            }
            let args: Vec<String> = words[first + 1..].iter().map(|w| w.text.clone()).collect();
            if program == "sed" && !printing_sed(&args) {
                return false;
            }
            uses_engine |= ENGINE_COMMANDS.contains(&program);
        }
    }
    uses_engine
}

fn session_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            session_files(&path, out);
        } else if kind.is_file() && path.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn recorded_commands(root: &Path) -> (Vec<(PathBuf, String)>, usize) {
    let mut files = Vec::new();
    session_files(root, &mut files);
    files.sort();
    let mut commands = Vec::new();
    let mut all_bash = 0;
    for file in files {
        let Ok(file) = std::fs::File::open(file) else {
            continue;
        };
        for record in BufReader::new(file).split(b'\n') {
            let Ok(bytes) = record else { break };
            let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let cwd = value.get("cwd").and_then(Value::as_str).unwrap_or("");
            let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
                continue;
            };
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_use")
                    || block.get("name").and_then(Value::as_str) != Some("Bash")
                {
                    continue;
                }
                let Some(command) = block.pointer("/input/command").and_then(Value::as_str) else {
                    continue;
                };
                all_bash += 1;
                if is_safe_and_in_scope(command) {
                    commands.push((PathBuf::from(cwd), command.to_string()));
                }
            }
        }
    }
    (commands, all_bash)
}

struct Fixture(PathBuf);

impl Fixture {
    fn create() -> std::io::Result<Self> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("kiro-replay-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&path)?;
        let fixture = Fixture(path);
        std::fs::create_dir(fixture.0.join("src"))?;
        std::fs::write(fixture.0.join("src/app.ts"),
            "export function retry(id: number) {\n  const attempts = 3;\n  return id + attempts;\n}\n")?;
        std::fs::write(
            fixture.0.join("notes.txt"),
            "alpha: first\nbeta: second\ngamma: third\n",
        )?;
        Ok(fixture)
    }

    fn commands(&self) -> Vec<(PathBuf, String)> {
        [
            "cat src/app.ts",
            "cat -n notes.txt",
            "sed -n '2,3p' src/app.ts",
            "head -2 notes.txt",
            "tail -n +2 notes.txt",
            "wc -l src/app.ts notes.txt",
            "cut -d: -f1 notes.txt",
            "ls src",
            "grep -n export src/app.ts",
            "grep -rn return src",
            "grep -F attempts src/app.ts",
            "grep -n missing src/app.ts",
            "cat absent.txt",
            "cat src/app.ts | grep -n const | head -1",
            "cd src && cat app.ts",
        ]
        .iter()
        .map(|command| (self.0.clone(), command.to_string()))
        .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn quote_word(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

struct McpClient {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl McpClient {
    fn start(binary: &Path, project: &Path) -> Option<Self> {
        let mut child = Command::new(binary)
            .args(["mcp", "--project"])
            .arg(project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let (send, lines) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut bytes = Vec::new();
                match Read::by_ref(&mut reader)
                    .take(MAX_BYTES as u64 + 1)
                    .read_until(b'\n', &mut bytes)
                {
                    Ok(0) | Err(_) => break,
                    Ok(_) if bytes.len() > MAX_BYTES => break,
                    Ok(_) => {}
                }
                let Ok(line) = String::from_utf8(bytes) else {
                    break;
                };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let mut client = Self {
            child,
            stdin,
            lines,
            next_id: 1,
        };
        client.request("initialize", serde_json::json!({"protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"kiro-replay","version":"1"}}))?;
        use std::io::Write;
        writeln!(
            client.stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .ok()?;
        Some(client)
    }

    fn request(&mut self, method: &str, params: Value) -> Option<Value> {
        use std::io::Write;
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .ok()?;
        self.stdin.flush().ok()?;
        let line = self.lines.recv_timeout(TIME_LIMIT).ok()?;
        let answer: Value = serde_json::from_str(&line).ok()?;
        (answer.get("id").and_then(Value::as_u64) == Some(id)).then_some(answer)
    }

    fn run(&mut self, cwd: &Path, command: &str) -> Option<String> {
        let command = format!("cd {} && {}", quote_word(&cwd.to_string_lossy()), command);
        let answer = self.request(
            "tools/call",
            serde_json::json!({"name":"run","arguments":{"command":command,"exact":true}}),
        )?;
        let result = answer.get("result")?;
        if result.get("isError").and_then(Value::as_bool) != Some(false) {
            return None;
        }
        result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .map(String::from)
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn exact_match(reference: &Run, native: &Reply) -> bool {
    !reference.truncated
        && reference.code == Some(native.code)
        && reference.stdout == native.stdout
        && reference.stderr == native.stderr.as_bytes()
}

/// MCP is a formatted UTF-8 text, not separate byte streams. Never normalize its newlines.
fn expected_mcp_text(reference: &Run) -> Option<String> {
    let code = reference.code?;
    let mut text = String::from_utf8(reference.stdout.clone()).ok()?;
    text.push_str(std::str::from_utf8(&reference.stderr).ok()?);
    if code != 0 {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("[exit code {code}]"));
    }
    Some(text)
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

fn main() -> ExitCode {
    let mut sessions = None;
    let mut fixture_mode = false;
    let mut max = 3000;
    let mut examples = 3;
    let mut mcp_binary = None;
    let mut project = None;
    let mut budget = Duration::from_secs(240);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixture" => fixture_mode = true,
            "--sessions" => sessions = args.next().map(PathBuf::from),
            "--max" => max = args.next().and_then(|x| x.parse().ok()).unwrap_or(max),
            "--examples" => examples = args.next().and_then(|x| x.parse().ok()).unwrap_or(examples),
            "--mcp" => mcp_binary = args.next().map(PathBuf::from),
            "--project" => project = args.next().map(PathBuf::from),
            "--budget-s" => {
                budget =
                    Duration::from_secs(args.next().and_then(|x| x.parse().ok()).unwrap_or(240))
            }
            _ => {
                eprintln!("unknown argument: {arg}; use --fixture or --sessions DIR [--project DIR] [--mcp BINARY]");
                return ExitCode::from(2);
            }
        }
    }
    if fixture_mode && (sessions.is_some() || project.is_some()) {
        eprintln!("--fixture cannot be combined with --sessions or --project");
        return ExitCode::from(2);
    }
    let fixture = if fixture_mode {
        Some(Fixture::create().expect("cannot create fixture"))
    } else {
        None
    };
    let (mut commands, all_bash) = if let Some(fixture) = &fixture {
        let commands = fixture.commands();
        let n = commands.len();
        (commands, n)
    } else if let Some(root) = sessions {
        recorded_commands(&root)
    } else {
        eprintln!(
            "choose --fixture for the public benchmark, or --sessions DIR to replay recorded calls"
        );
        return ExitCode::from(2);
    };
    let project = fixture.as_ref().map(|f| f.0.clone()).or(project);
    if let Some(project) = &project {
        commands.retain(|(cwd, _)| cwd.starts_with(project));
    }
    let eligible = commands.len();
    if commands.len() > max {
        let step = commands.len() as f64 / max.max(1) as f64;
        commands = (0..max)
            .map(|k| commands[(k as f64 * step) as usize].clone())
            .collect();
    }
    let mut mcp = match (&mcp_binary, &project) {
        (Some(binary), Some(project)) => {
            Some(McpClient::start(binary, project).expect("cannot start MCP"))
        }
        (Some(_), None) => {
            eprintln!("--mcp needs --project, or --fixture");
            return ExitCode::from(2);
        }
        _ => None,
    };
    let started = Instant::now();
    let (mut replayed, mut identical, mut different, mut unavailable) = (0, 0, 0, 0);
    let mut refused = BTreeMap::<String, usize>::new();
    let (mut shell_times, mut engine_times) = (Vec::new(), Vec::new());
    for (cwd, command) in &commands {
        if started.elapsed() > budget {
            break;
        }
        if !cwd.is_dir() {
            unavailable += 1;
            continue;
        }
        if !is_safe_and_in_scope(command) {
            *refused
                .entry("syntax or program not allowlisted".into())
                .or_default() += 1;
            continue;
        }
        // Preflight comes BEFORE the shell, including its warm-up run. A refused command
        // never reaches the reference shell, regardless of its apparent program name.
        let mut session = Session { dir: cwd.clone() };
        if let Err(reason) = engine::run_native(command, &mut session) {
            *refused.entry(reason).or_default() += 1;
            continue;
        }
        if run_reference(command, cwd).is_none() {
            unavailable += 1;
            continue;
        }
        // Measure both only after preflight and shell warm-up have populated the cache.
        let mut session = Session { dir: cwd.clone() };
        let begin = Instant::now();
        let native = match engine::run_native(command, &mut session) {
            Ok(reply) => reply,
            Err(reason) => {
                *refused.entry(reason).or_default() += 1;
                continue;
            }
        };
        let native_time = begin.elapsed();
        let Some(reference) = run_reference(command, cwd) else {
            unavailable += 1;
            continue;
        };
        if reference.truncated {
            unavailable += 1;
            continue;
        }
        replayed += 1;
        let (transport_same, engine_time) = if let Some(client) = &mut mcp {
            let expected = expected_mcp_text(&reference);
            let begin = Instant::now();
            let got = client.run(cwd, command);
            (expected.is_some() && got == expected, begin.elapsed())
        } else {
            (true, native_time)
        };
        if !exact_match(&reference, &native) || !transport_same {
            different += 1;
            if different <= examples {
                eprintln!("different output/status (excluded from timings): {command}");
            }
            continue;
        }
        identical += 1;
        shell_times.push(reference.elapsed.as_secs_f64() * 1000.0);
        engine_times.push(engine_time.as_secs_f64() * 1000.0);
    }
    println!("ENGINE REPLAY — reference: /bin/sh, system PATH, warm file cache");
    println!(
        "Recorded/public commands: {all_bash}; syntax-eligible: {eligible}; sampled: {}",
        commands.len()
    );
    println!(
        "Refused BEFORE shell replay: {}",
        refused.values().sum::<usize>()
    );
    for (reason, count) in &refused {
        println!("  {count}: {reason}");
    }
    println!("Replayed: {replayed}; skipped/unavailable: {unavailable}");
    println!("Exact stdout + stderr + exit status (native): {identical}; DIFFERENT: {different}");
    if mcp.is_some() {
        println!("MCP additionally checks formatted UTF-8 response equivalence; this is not a separate byte-stream/status proof.");
    }
    println!("Timings include ONLY the {identical} fully equivalent cases; no reordered lines or normalized errors.");
    println!(
        "Total shell: {:.3} ms; engine: {:.3} ms",
        shell_times.iter().sum::<f64>(),
        engine_times.iter().sum::<f64>()
    );
    println!(
        "Median shell: {:.3} ms; engine: {:.3} ms",
        median(&mut shell_times),
        median(&mut engine_times)
    );
    if started.elapsed() > budget {
        println!("Time budget reached; remaining commands were not replayed.");
    }
    ExitCode::from(u8::from(
        different > 0 || (fixture_mode && (identical != commands.len() || commands.is_empty())),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_rejects_writers_even_in_skipped_branches() {
        for command in [
            "sed -n 'w out.txt' notes.txt",
            "sed -n '1p;w out.txt' notes.txt",
            "sed -n -e 'e touch out.txt' notes.txt",
            "cat notes.txt | sort -o out.txt",
            "cat notes.txt > out.txt",
            "/some/custom/cat notes.txt",
            "cat notes.txt || touch out.txt",
            "grep missing notes.txt && sed -n 'w out.txt' notes.txt",
            "cat $(touch out.txt)",
        ] {
            assert!(!is_safe_and_in_scope(command), "{command}");
        }
    }

    #[test]
    fn public_fixture_commands_are_allowlisted() {
        let fixture = Fixture::create().unwrap();
        for (_, command) in fixture.commands() {
            assert!(is_safe_and_in_scope(&command), "{command}");
        }
    }

    #[test]
    fn exact_comparison_rejects_status_stderr_order_and_newline_changes() {
        let reference = Run {
            stdout: b"a\nb\n".to_vec(),
            stderr: b"error\n".to_vec(),
            code: Some(1),
            elapsed: Duration::ZERO,
            truncated: false,
        };
        let mut native = Reply {
            stdout: reference.stdout.clone(),
            stderr: "error\n".into(),
            code: 1,
            native: true,
            spawned: 0,
            fallback_reason: String::new(),
        };
        assert!(exact_match(&reference, &native));
        native.code = 0;
        assert!(!exact_match(&reference, &native));
        native.code = 1;
        native.stdout = b"b\na\n".to_vec();
        assert!(!exact_match(&reference, &native));
        native.stdout = reference.stdout.clone();
        native.stderr = "error".into();
        assert!(!exact_match(&reference, &native));
        assert_eq!(
            expected_mcp_text(&reference).unwrap(),
            "a\nb\nerror\n[exit code 1]"
        );
    }
}
