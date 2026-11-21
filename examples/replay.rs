//! Replay benchmark: the engine against the regular terminal, on real commands.
//!
//! Takes the read/search commands that AI agents really ran (from the session files Claude
//! Code records under `~/.claude/projects`) and replays each one three ways:
//!
//! 1. the regular terminal: a fresh shell, then the programs;
//! 2. optionally, the same with [RTK](https://github.com/rtk-ai/rtk) in front (its hook, then
//!    its wrappers), when `--rtk DIR` is given;
//! 3. kiro, either in-process or, with `--mcp PATH --project DIR`, through its MCP server
//!    exactly as an agent would call it.
//!
//! For each way it reports the programs started, the time and the size of the output, and it
//! checks that kiro returns exactly what the regular terminal returns.
//!
//! Only commands that are read-only by construction are replayed: lists and pipelines made
//! solely of known read-only programs, with no redirection to a file and no substitution.
//!
//! ```text
//! cargo run --release --example replay -- --max 600
//! cargo run --release --example replay -- --project ~/code/app --mcp target/release/kiro
//! ```

use kiro::engine::{self, Session};
use kiro::shell;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIME_LIMIT: Duration = Duration::from_secs(8);
const MAX_BYTES: usize = 4_000_000;
const ENGINE_COMMANDS: &[&str] = &[
    "cat", "sed", "head", "tail", "wc", "grep", "egrep", "fgrep", "cut", "ls",
];
const SAFE_FILTERS: &[&str] = &[
    "cat", "sed", "head", "tail", "wc", "grep", "egrep", "fgrep", "sort", "uniq", "cut", "tr",
    "nl", "rev", "column", "cd", "echo", "ls", "pwd", "true",
];

struct Run {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: Option<i32>,
    elapsed: Duration,
    truncated: bool,
}

/// Runs a program with a time limit; `None` when it does not start or runs too long.
fn run_with_timeout(argv: &[&str], dir: &Path, env: &[(String, String)]) -> Option<Run> {
    let (program, args) = argv.split_first()?;
    let start = Instant::now();
    let mut child = Command::new(program)
        .args(args)
        .current_dir(dir)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut error_pipe = child.stderr.take()?;
    let error_reader = std::thread::spawn(move || {
        let mut buffer: Vec<u8> = Vec::new();
        let _ = error_pipe.read_to_end(&mut buffer);
        buffer.truncate(MAX_BYTES);
        buffer
    });
    let mut pipe = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer: Vec<u8> = Vec::new();
        let mut block = [0u8; 65536];
        let mut truncated = false;
        loop {
            match pipe.read(&mut block) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = MAX_BYTES.saturating_sub(buffer.len());
                    let taken = n.min(room);
                    buffer.extend_from_slice(&block[..taken]);
                    if taken < n {
                        truncated = true;
                    }
                }
            }
        }
        (buffer, truncated)
    });
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if start.elapsed() > TIME_LIMIT {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_micros(200));
            }
            Err(_) => break None,
        }
    };
    let elapsed = start.elapsed();
    let (stdout, truncated) = reader.join().ok()?;
    let stderr = error_reader.join().ok()?;
    if timed_out {
        return None;
    }
    Some(Run {
        stdout,
        stderr,
        code: status?.code(),
        elapsed,
        truncated,
    })
}

fn first_word(stage: &str) -> &str {
    for word in stage.split_whitespace() {
        if word == "command" {
            continue;
        }
        return word.rsplit('/').next().unwrap_or(word);
    }
    ""
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

/// Read-only by construction: lists (`;`, `&&`, `||`) and pipelines made only of known
/// read-only programs, with no redirection to a file and no substitution.
fn is_safe_and_in_scope(command: &str) -> bool {
    let bare = shell::outside_quotes(command)
        .replace("2>&1", "")
        .replace("2>/dev/null", "");
    if bare.contains(['`', '>', '<']) || bare.contains("$(") {
        return false;
    }
    let Some(items) = shell::split_list(command) else {
        return false;
    };
    let mut uses_engine = false;
    for (_, item) in &items {
        for stage in stages_of(item) {
            let w = first_word(stage);
            // `sed` is only read-only in its printing form.
            let printing_sed = stage.contains(" -n") && !stage.contains("-i");
            if !SAFE_FILTERS.contains(&w) || (w == "sed" && !printing_sed) {
                return false;
            }
            if ENGINE_COMMANDS.contains(&w) {
                uses_engine = true;
            }
        }
    }
    uses_engine
}

fn session_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            session_files(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
}

/// Collects (working directory, command) for every Bash call in the recorded sessions.
fn recorded_commands(root: &Path) -> (Vec<(String, String)>, usize) {
    let mut files = Vec::new();
    session_files(root, &mut files);
    files.sort();
    let mut commands = Vec::new();
    let mut all_bash = 0usize;
    for file in files {
        let Ok(f) = std::fs::File::open(&file) else {
            continue;
        };
        for line in BufReader::with_capacity(1 << 20, f).split(b'\n') {
            let Ok(bytes) = line else { continue };
            if bytes.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("");
            let Some(Value::Array(blocks)) = v.get("message").and_then(|m| m.get("content")) else {
                continue;
            };
            for b in blocks {
                if b.get("type").and_then(Value::as_str) != Some("tool_use")
                    || b.get("name").and_then(Value::as_str) != Some("Bash")
                {
                    continue;
                }
                let Some(command) = b
                    .get("input")
                    .and_then(|i| i.get("command"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                all_bash += 1;
                if is_safe_and_in_scope(command) {
                    commands.push((cwd.to_string(), command.to_string()));
                }
            }
        }
    }
    (commands, all_bash)
}

fn sorted_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    lines.sort();
    lines
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values[values.len() / 2]
}

/// A client for the `kiro mcp` server: one long-lived process, one JSON message per line.
struct McpClient {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn start(binary: &Path, project: &Path) -> Option<McpClient> {
        let mut child = Command::new(binary)
            .arg("mcp")
            .arg("--project")
            .arg(project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = BufReader::new(child.stdout.take()?);
        let mut client = McpClient {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        client.request("initialize", serde_json::json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "kiro-replay", "version": "0"}}))?;
        use std::io::Write;
        writeln!(
            client.stdin,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
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
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .ok()?;
        self.stdin.flush().ok()?;
        let mut line = String::new();
        self.stdout.read_line(&mut line).ok()?;
        serde_json::from_str(&line).ok()
    }

    /// Calls the `run` tool. Returns (text, refused). `exact` asks for the byte-for-byte output.
    fn run(&mut self, command: &str, exact: bool) -> Option<(String, bool)> {
        let answer = self.request(
            "tools/call",
            serde_json::json!({"name": "run", "arguments": {"command": command, "exact": exact}}),
        )?;
        let result = answer.get("result")?;
        let text = result
            .get("content")?
            .get(0)?
            .get("text")?
            .as_str()?
            .to_string();
        Some((
            text,
            result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Splits the `[exit code N]` marker the server appends on failure from the rest of the text.
fn split_exit_code(text: &str) -> (&str, i32) {
    if let Some(at) = text.rfind("[exit code ") {
        if let Some(n) = text[at + 11..]
            .strip_suffix(']')
            .and_then(|x| x.parse::<i32>().ok())
        {
            return (&text[..at], n);
        }
    }
    (text, 0)
}

fn main() {
    let mut root =
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude/projects");
    let mut max = 3000usize;
    let mut rtk_dir: Option<PathBuf> = None;
    let mut examples = 0usize;
    let mut mcp_binary: Option<PathBuf> = None;
    let mut project: Option<PathBuf> = None;
    let mut budget = Duration::from_secs(240);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--sessions" => root = PathBuf::from(args.next().unwrap_or_default()),
            "--max" => max = args.next().and_then(|x| x.parse().ok()).unwrap_or(max),
            "--rtk" => rtk_dir = args.next().map(PathBuf::from),
            "--examples" => examples = args.next().and_then(|x| x.parse().ok()).unwrap_or(0),
            "--mcp" => mcp_binary = args.next().map(PathBuf::from),
            "--project" => project = args.next().map(PathBuf::from),
            "--budget-s" => {
                budget =
                    Duration::from_secs(args.next().and_then(|x| x.parse().ok()).unwrap_or(240))
            }
            other => {
                eprintln!("unknown argument: {}", other);
                std::process::exit(2);
            }
        }
    }
    // The regular terminal: the user's own shell.
    let shell_path = ["KIRO_SHELL", "SHELL"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string());
    let started = Instant::now();
    let (mut commands, all_bash) = recorded_commands(&root);
    if let Some(p) = &project {
        // Only the commands that were run inside this project.
        let prefix = p.to_string_lossy().into_owned();
        commands.retain(|(cwd, _)| cwd.starts_with(&prefix));
    }
    let mut mcp = match (&mcp_binary, &project) {
        (Some(binary), Some(p)) => {
            Some(McpClient::start(binary, p).expect("cannot start the MCP server"))
        }
        (Some(_), None) => {
            eprintln!("--mcp needs --project");
            std::process::exit(2);
        }
        _ => None,
    };
    let in_scope = commands.len();
    if commands.len() > max {
        // Even sampling across the whole history, not just the first sessions.
        let step = commands.len() as f64 / max as f64;
        commands = (0..max)
            .map(|k| commands[(k as f64 * step) as usize].clone())
            .collect();
    }
    let mut rtk_env: Vec<(String, String)> =
        vec![("RTK_TELEMETRY_DISABLED".to_string(), "1".to_string())];
    if let Some(d) = &rtk_dir {
        rtk_env.push((
            "RTK_DB_PATH".to_string(),
            d.join("rtk-replay.db").to_string_lossy().into_owned(),
        ));
    }

    let (mut replayed, mut missing_dir, mut too_slow) = (0usize, 0usize, 0usize);
    let (mut native, mut identical, mut reordered, mut different, mut code_mismatch) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut fallback_reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut different_cases: Vec<String> = Vec::new();
    // Over the commands the engine answered correctly:
    let (mut t_shell, mut t_rtk, mut t_engine) = (Vec::new(), Vec::new(), Vec::new());
    let (mut spawned_shell, mut spawned_rtk) = (0u64, 0u64);
    let (mut chars_shell, mut chars_rtk, mut chars_engine) = (0u64, 0u64, 0u64);
    let mut rtk_rewritten = 0usize;

    let mut not_reached = 0usize;
    for (k, (cwd, command)) in commands.iter().enumerate() {
        if started.elapsed() > budget {
            not_reached = commands.len() - k;
            break;
        }
        let dir = Path::new(cwd);
        if !dir.is_dir() {
            missing_dir += 1;
            continue;
        }
        // A command that starts with `cd` into a directory that no longer exists cannot be
        // replayed either: each terminal would only print its own "no such directory" message.
        let mut target = dir.to_path_buf();
        let mut rest = command.as_str();
        let mut reachable = true;
        loop {
            let (cd, after) = shell::leading_cd(rest);
            let Some(d) = cd else { break };
            target = if d == "~" || d.starts_with("~/") {
                PathBuf::from(format!(
                    "{}{}",
                    std::env::var("HOME").unwrap_or_default(),
                    &d[1..]
                ))
            } else if Path::new(&d).is_absolute() {
                PathBuf::from(&d)
            } else {
                target.join(&d)
            };
            if !target.is_dir() {
                reachable = false;
                break;
            }
            rest = after;
        }
        if !reachable {
            missing_dir += 1;
            continue;
        }
        // Warm the file cache once, so no path pays for cold disk reads.
        if run_with_timeout(&[shell_path.as_str(), "-c", command], dir, &[]).is_none() {
            too_slow += 1;
            continue;
        }
        let Some(reference) = run_with_timeout(&[shell_path.as_str(), "-c", command], dir, &[])
        else {
            too_slow += 1;
            continue;
        };
        if reference.truncated {
            too_slow += 1;
            continue;
        }
        replayed += 1;

        // The engine: either in-process, or through the MCP server like an AI agent would.
        let begin = Instant::now();
        let reply: Result<(Vec<u8>, i32), String> = match &mut mcp {
            Some(client) => {
                let sent = format!("cd {} && {}", cwd, command);
                match client.run(&sent, true) {
                    Some((text, true)) => Err(text
                        .split('(')
                        .nth(1)
                        .and_then(|x| x.split(')').next())
                        .unwrap_or("refused")
                        .to_string()),
                    Some((text, false)) => {
                        let (body, code) = split_exit_code(&text);
                        Ok((body.as_bytes().to_vec(), code))
                    }
                    None => Err("no answer from the MCP server".to_string()),
                }
            }
            None => {
                let mut session = Session {
                    dir: dir.to_path_buf(),
                };
                engine::run_native(command, &mut session).map(|r| (r.stdout, r.code))
            }
        };
        let engine_time = begin.elapsed();
        struct Answer {
            stdout: Vec<u8>,
            code: i32,
        }
        let reply = match reply {
            Ok((stdout, code)) => Answer { stdout, code },
            Err(reason) => {
                let key: String = reason.chars().take(48).collect();
                *fallback_reasons.entry(key).or_default() += 1;
                continue;
            }
        };
        native += 1;
        // Through MCP the answer is one UTF-8 text: the output, then the error text.
        let same = if mcp.is_some() {
            let expected = format!(
                "{}{}",
                String::from_utf8_lossy(&reference.stdout),
                String::from_utf8_lossy(&reference.stderr)
            );
            let got = String::from_utf8_lossy(&reply.stdout);
            got == expected
                || (reply.code != 0
                    && got.trim_end_matches('\n') == expected.trim_end_matches('\n'))
        } else {
            reply.stdout == reference.stdout
        };
        let same_lines = same || sorted_lines(&reply.stdout) == sorted_lines(&reference.stdout);
        if same {
            identical += 1;
        } else if same_lines {
            reordered += 1;
        } else {
            different += 1;
            if different_cases.len() < examples {
                let shown = |b: &[u8]| String::from_utf8_lossy(&b[..b.len().min(300)]).into_owned();
                different_cases.push(format!(
                    "  $ {}\n    regular: {:?}\n    engine : {:?}",
                    command,
                    shown(&reference.stdout),
                    shown(&reply.stdout)
                ));
            }
            continue;
        }
        if reference.code != Some(reply.code) {
            code_mismatch += 1;
        }

        // RTK: its hook rewrites the command, then the shell runs the rewritten one.
        let stages = stages_of(command).len() as u64;
        let mut rtk_time = Duration::ZERO;
        let mut rtk_chars = reference.stdout.len() as u64;
        if rtk_dir.is_some() {
            if let Some(hook) = run_with_timeout(&["rtk", "rewrite", command], dir, &rtk_env) {
                rtk_time += hook.elapsed;
                let rewritten = String::from_utf8_lossy(&hook.stdout).trim().to_string();
                let effective = if rewritten.is_empty() {
                    command.clone()
                } else {
                    rewritten
                };
                if effective != *command {
                    rtk_rewritten += 1;
                }
                if let Some(r) =
                    run_with_timeout(&[shell_path.as_str(), "-c", &effective], dir, &rtk_env)
                {
                    rtk_time += r.elapsed;
                    if !r.stdout.is_empty() || reference.stdout.is_empty() {
                        rtk_chars = r.stdout.len() as u64;
                    }
                }
            }
        }
        // What the agent really receives from the engine: search listings grouped by file.
        chars_engine += match &mut mcp {
            Some(client) => client
                .run(&format!("cd {} && {}", cwd, command), false)
                .map_or(reference.stdout.len(), |(text, _)| text.len())
                as u64,
            None => reference.stdout.len() as u64,
        };
        t_shell.push(reference.elapsed.as_secs_f64() * 1000.0);
        t_rtk.push(rtk_time.as_secs_f64() * 1000.0);
        t_engine.push(engine_time.as_secs_f64() * 1000.0);
        spawned_shell += 1 + stages;
        spawned_rtk += 2 + stages;
        chars_shell += reference.stdout.len() as u64;
        chars_rtk += rtk_chars;
    }

    let pct = |a: usize, b: usize| {
        if b == 0 {
            "n/a".to_string()
        } else {
            format!("{:.1} %", 100.0 * a as f64 / b as f64)
        }
    };
    println!(
        "ENGINE REPLAY ({:.0} s){}",
        started.elapsed().as_secs_f64(),
        if mcp.is_some() {
            " - every command sent through the MCP server"
        } else {
            ""
        }
    );
    if not_reached > 0 {
        println!(
            "time budget reached: {} sampled commands were not replayed",
            not_reached
        );
    }
    println!("Bash commands in the recorded sessions : {}", all_bash);
    println!(
        "  read/search commands, safe to replay : {} ({})",
        in_scope,
        pct(in_scope, all_bash)
    );
    println!(
        "  sampled {} ; replayed {} ; skipped: {} directory gone, {} too slow or too large",
        commands.len(),
        replayed,
        missing_dir,
        too_slow
    );
    println!(
        "Handled inside the engine              : {} of {} ({})",
        native,
        replayed,
        pct(native, replayed)
    );
    println!(
        "  identical output, byte for byte      : {} ({})",
        identical,
        pct(identical, native)
    );
    println!(
        "  same lines, different file order     : {} ({})",
        reordered,
        pct(reordered, native)
    );
    println!(
        "  DIFFERENT output                     : {} ({})",
        different,
        pct(different, native)
    );
    println!("  exit code differs                    : {}", code_mismatch);
    println!(
        "Handed to the regular shell            : {} ({})",
        replayed - native,
        pct(replayed - native, replayed)
    );
    let mut reasons: Vec<(&String, &usize)> = fallback_reasons.iter().collect();
    reasons.sort_by(|a, b| b.1.cmp(a.1));
    for (reason, n) in reasons.iter().take(12) {
        println!("    {:>5}  {}", n, reason);
    }

    let n = t_engine.len();
    let sum = |v: &[f64]| v.iter().sum::<f64>();
    println!("\nOver the {} commands the engine answered correctly:", n);
    println!(
        "{:<34} {:>12} {:>12} {:>12}",
        "", "regular", "with RTK", "new engine"
    );
    println!(
        "{:<34} {:>12} {:>12} {:>12}",
        "programs started",
        spawned_shell,
        if rtk_dir.is_some() {
            spawned_rtk.to_string()
        } else {
            "-".to_string()
        },
        0
    );
    println!(
        "{:<34} {:>10.0} ms {:>10.0} ms {:>10.1} ms",
        "total time",
        sum(&t_shell),
        sum(&t_rtk),
        sum(&t_engine)
    );
    println!(
        "{:<34} {:>10.2} ms {:>10.2} ms {:>10.3} ms",
        "median time per command",
        median(&mut t_shell),
        median(&mut t_rtk),
        median(&mut t_engine)
    );
    println!(
        "{:<34} {:>12} {:>12} {:>12}",
        "output characters",
        chars_shell,
        if rtk_dir.is_some() {
            chars_rtk.to_string()
        } else {
            "-".to_string()
        },
        chars_engine
    );
    if rtk_dir.is_some() {
        println!("RTK rewrote {} of these {} commands", rtk_rewritten, n);
    }
    if !different_cases.is_empty() {
        println!("\nCases where the engine output differs:");
        for case in &different_cases {
            println!("{}", case);
        }
    }
}
