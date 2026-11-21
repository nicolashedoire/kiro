//! Live comparison: the same task, run by a real Claude Code session, with and without kiro.
//!
//! Runs a task several times, alternating two setups:
//!
//! - `regular`: Claude Code as it is on this machine (its shell tool, its hooks);
//! - `engine`: the same, plus kiro served over MCP (`kiro mcp`).
//!
//! File edits are disabled in both, sessions are not saved, and each session is capped at
//! half a dollar. For each run it reports whether the answer is right (it must mention every
//! expected string), the number of round trips with the model, the time, the tokens and the
//! cost, and how the agent used its tools.
//!
//! This costs real model calls. Results vary a lot from one session to the next: run several
//! repetitions and several tasks before concluding anything.
//!
//! ```text
//! cargo run --release --example live -- <project dir> <path to kiro> <output dir> \
//!     <repetitions> <expected,expected,...> [regular|engine] < task.txt
//! ```

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Default, Clone)]
struct Outcome {
    setup: String,
    finished: bool,
    correct: bool,
    turns: u64,
    seconds: f64,
    cost: f64,
    tokens: u64,
    output_tokens: u64,
    shell_calls: u64,
    read_calls: u64,
    search_calls: u64,
    engine_calls: u64,
    engine_commands: u64,
    engine_refused: u64,
    scout_calls: u64,
    tool_chars: u64,
    denied: u64,
    answer: String,
}

fn result_length(content: &Value) -> usize {
    match content {
        Value::String(s) => s.len(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .map(str::len)
            .sum(),
        _ => 0,
    }
}

fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn run_claude(
    setup: &str,
    project: &Path,
    task: &str,
    mcp_config: &Path,
    with_engine: bool,
    stream_file: &Path,
) -> Outcome {
    let mut o = Outcome {
        setup: setup.to_string(),
        ..Outcome::default()
    };
    let mut command = Command::new("claude");
    command
        .current_dir(project)
        .arg("-p")
        .args(["--output-format", "stream-json", "--verbose"])
        .args(["--model", "sonnet"])
        .arg("--no-session-persistence")
        .arg("--strict-mcp-config")
        .arg("--mcp-config")
        .arg(mcp_config)
        .args(["--max-budget-usd", "0.50"])
        .args(["--permission-mode", "auto"])
        .args(["--disallowedTools", "Edit,Write,NotebookEdit,Agent,Task"]);
    if with_engine {
        command.args(["--allowedTools", "mcp__kiro__run,mcp__kiro__scout"]);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let start = Instant::now();
    let Ok(mut child) = command.spawn() else {
        o.answer = "cannot start claude".to_string();
        return o;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(task.as_bytes());
    }
    let mut pipe = child.stdout.take().expect("stdout");
    let reader = std::thread::spawn(move || {
        let mut all = String::new();
        let _ = pipe.read_to_string(&mut all);
        all
    });
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(600) {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => break,
        }
    }
    let all = reader.join().unwrap_or_default();
    let _ = std::fs::write(stream_file, &all);
    o.seconds = start.elapsed().as_secs_f64();

    for line in all.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(Value::Array(blocks)) = v.get("message").and_then(|m| m.get("content"))
                else {
                    continue;
                };
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    match b.get("name").and_then(Value::as_str).unwrap_or("") {
                        "Bash" => o.shell_calls += 1,
                        "Read" => o.read_calls += 1,
                        "Grep" | "Glob" => o.search_calls += 1,
                        "mcp__kiro__scout" => o.scout_calls += 1,
                        "mcp__kiro__run" => {
                            o.engine_calls += 1;
                            let input = b.get("input");
                            let several = input
                                .and_then(|i| i.get("commands"))
                                .and_then(Value::as_array)
                                .map_or(0, Vec::len)
                                as u64;
                            let single = input.and_then(|i| i.get("command")).is_some() as u64;
                            o.engine_commands += several + single;
                        }
                        _ => {}
                    }
                }
            }
            Some("user") => {
                let Some(Value::Array(blocks)) = v.get("message").and_then(|m| m.get("content"))
                else {
                    continue;
                };
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let Some(content) = b.get("content") else {
                        continue;
                    };
                    o.tool_chars += result_length(content) as u64;
                    let text = result_text(content);
                    if text.contains("[not run: the fast terminal") {
                        o.engine_refused += 1;
                    }
                    if b.get("is_error").and_then(Value::as_bool) == Some(true)
                        && (text.contains("permission")
                            || text.contains("denied")
                            || text.contains("not allowed"))
                    {
                        o.denied += 1;
                    }
                }
            }
            Some("result") => {
                o.finished = v.get("subtype").and_then(Value::as_str) == Some("success");
                o.turns = v.get("num_turns").and_then(Value::as_u64).unwrap_or(0);
                o.cost = v
                    .get("total_cost_usd")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                if let Some(ms) = v.get("duration_ms").and_then(Value::as_f64) {
                    o.seconds = ms / 1000.0;
                }
                o.answer = v
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(u) = v.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    o.output_tokens = n("output_tokens");
                    o.tokens = n("input_tokens")
                        + n("cache_creation_input_tokens")
                        + n("cache_read_input_tokens")
                        + o.output_tokens;
                }
            }
            _ => {}
        }
    }
    o
}

fn print_row(o: &Outcome) {
    println!(
        "{:<8} {:<6} {:>3} trips {:>6.0} s {:>8} tok {:>6} out {:>6.3} $ | shell {:>2} read {:>2} search {:>2} scout {:>2} engine {:>2} calls / {:>2} commands ({} refused) | {:>7} chars | denied {}",
        o.setup,
        if !o.finished { "STOP" } else if o.correct { "right" } else { "WRONG" },
        o.turns,
        o.seconds,
        o.tokens,
        o.output_tokens,
        o.cost,
        o.shell_calls,
        o.read_calls,
        o.search_calls,
        o.scout_calls,
        o.engine_calls,
        o.engine_commands,
        o.engine_refused,
        o.tool_chars,
        o.denied
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 5 {
        eprintln!("usage: live <project dir> <path to kiro> <output dir> <repetitions> <expected,expected,...> [regular|engine] < task.txt");
        std::process::exit(2);
    }
    let project = PathBuf::from(&args[0]);
    let binary = PathBuf::from(&args[1])
        .canonicalize()
        .expect("kiro binary not found");
    std::fs::create_dir_all(&args[2]).expect("output dir");
    let out = PathBuf::from(&args[2]).canonicalize().expect("output dir");
    let repetitions: usize = args[3].parse().expect("repetitions");
    let expected: Vec<String> = args[4]
        .split(',')
        .map(|x| x.trim().to_lowercase())
        .filter(|x| !x.is_empty())
        .collect();
    let mut task = String::new();
    std::io::stdin()
        .read_to_string(&mut task)
        .expect("task on stdin");

    let regular_config = out.join("mcp-regular.json");
    std::fs::write(&regular_config, json!({ "mcpServers": {} }).to_string()).expect("write");
    let engine_config = out.join("mcp-engine.json");
    std::fs::write(
        &engine_config,
        json!({ "mcpServers": { "kiro": {
            "command": binary.to_string_lossy(),
            "args": ["mcp", "--project", project.to_string_lossy()]
        } } })
        .to_string(),
    )
    .expect("write");

    // Optional sixth argument: run only one setup (`regular` or `engine`).
    let only: Option<String> = args.get(5).cloned();
    let mut outcomes: Vec<Outcome> = Vec::new();
    for r in 1..=repetitions {
        for (setup, config, with_engine) in [
            ("regular", &regular_config, false),
            ("engine", &engine_config, true),
        ] {
            if only.as_deref().is_some_and(|o| o != setup) {
                continue;
            }
            let mut o = run_claude(
                setup,
                &project,
                &task,
                config,
                with_engine,
                &out.join(format!("{}-{}.jsonl", setup, r)),
            );
            let answer = o.answer.to_lowercase();
            o.correct = !answer.is_empty()
                && expected
                    .iter()
                    .all(|e| e.split('|').any(|x| answer.contains(x)));
            print_row(&o);
            outcomes.push(o);
        }
    }
    println!();
    for setup in ["regular", "engine"] {
        let group: Vec<&Outcome> = outcomes.iter().filter(|o| o.setup == setup).collect();
        let n = group.len().max(1) as f64;
        let mean = |f: &dyn Fn(&Outcome) -> f64| group.iter().map(|o| f(o)).sum::<f64>() / n;
        println!(
            "mean {:<8} right {}/{}  {:>5.1} trips  {:>6.1} s  {:>8.0} tokens  {:>6.0} output tokens  {:>6.3} $  {:>8.0} chars read",
            setup,
            group.iter().filter(|o| o.correct).count(),
            group.len(),
            mean(&|o| o.turns as f64),
            mean(&|o| o.seconds),
            mean(&|o| o.tokens as f64),
            mean(&|o| o.output_tokens as f64),
            mean(&|o| o.cost),
            mean(&|o| o.tool_chars as f64),
        );
    }
    println!(
        "total cost reported by Claude: {:.2} $",
        outcomes.iter().map(|o| o.cost).sum::<f64>()
    );
    let summary: Vec<Value> = outcomes
        .iter()
        .map(|o| json!({ "setup": o.setup, "finished": o.finished, "correct": o.correct, "trips": o.turns, "seconds": o.seconds, "cost_usd": o.cost, "tokens": o.tokens, "output_tokens": o.output_tokens,
            "shell_calls": o.shell_calls, "read_calls": o.read_calls, "search_calls": o.search_calls, "engine_calls": o.engine_calls, "engine_commands": o.engine_commands, "engine_refused": o.engine_refused,
            "tool_chars": o.tool_chars, "denied": o.denied, "answer": o.answer }))
        .collect();
    let _ = std::fs::write(
        out.join("results.json"),
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    );
}
