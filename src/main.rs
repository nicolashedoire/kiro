//! `kiro`: the command-line entry point.
//!
//! - `kiro run COMMAND` runs a command through the engine (and tells how, with `--stats`).
//! - `kiro mcp` serves the engine and the scout to an AI agent over MCP.
//! - `kiro scout TERM...` prints the map of an area of the code.

use kiro::diagnose;
use kiro::engine::{self, Session};
use kiro::scout::{self, Request};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

const HELP: &str = "\
kiro - a terminal engine for AI coding agents

USAGE:
    kiro run [--stats] COMMAND      run a command through the engine
    kiro scout [--project DIR] TERM...
                                    map where the terms live in the code
    kiro diagnose [--project DIR] --report FILE [TERM...]
                                    retrieve source evidence for an error or issue
        --message TEXT             supply the report directly instead of a file
        --json --budget N --max-files N --path PATH
                                    structured output and bounded evidence selection
    kiro mcp [--project DIR] [--tools run,scout,diagnose]
                                    serve selected tools to an AI agent (MCP, stdio)
    kiro --version
    kiro --help

Reading and searching (cat, sed -n, head, tail, wc, cut, ls, grep, pipelines and
`;` / `&&` / `||` lists of those) run inside the engine, with no shell and no program
started. `kiro run` hands anything else to the regular shell (KIRO_SHELL, then SHELL,
then /bin/sh). The MCP server never does: it refuses what the engine does not cover.
";

/// Removes `--name value` from the arguments and returns the value.
fn take_option(args: &mut Vec<String>, name: &str) -> Option<String> {
    let at = args.iter().position(|a| a == name)?;
    if at + 1 >= args.len() {
        return None;
    }
    let value = args.remove(at + 1);
    args.remove(at);
    Some(value)
}

/// Removes `--name` from the arguments and tells whether it was there.
fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    match args.iter().position(|a| a == name) {
        Some(at) => {
            args.remove(at);
            true
        }
        None => false,
    }
}

fn project_dir(args: &mut Vec<String>) -> PathBuf {
    take_option(args, "--project")
        .map(PathBuf::from)
        .unwrap_or_else(|| engine::current_dir().unwrap_or_default())
}

fn run(mut args: Vec<String>) -> ExitCode {
    let stats = take_flag(&mut args, "--stats");
    let command = args.join(" ");
    if command.trim().is_empty() {
        eprintln!("kiro run: the command is missing");
        return ExitCode::from(2);
    }
    let mut session = Session {
        dir: engine::current_dir().unwrap_or_default(),
    };
    let start = Instant::now();
    let reply = engine::run(&command, &mut session);
    let milliseconds = start.elapsed().as_secs_f64() * 1000.0;
    let _ = std::io::stdout().write_all(&reply.stdout);
    eprint!("{}", reply.stderr);
    if stats {
        if reply.native {
            eprintln!("[kiro] inside the engine, 0 program started, {milliseconds:.2} ms");
        } else {
            eprintln!(
                "[kiro] handed to the regular shell ({}), {} programs started, {milliseconds:.2} ms",
                reply.fallback_reason, reply.spawned
            );
        }
    }
    ExitCode::from(reply.code.clamp(0, 255) as u8)
}

fn diagnose_options(args: Vec<String>) -> Result<(PathBuf, diagnose::Request, bool), String> {
    let mut project = engine::current_dir().map_err(|e| e.to_string())?;
    let mut report_path = None;
    let mut message = None;
    let mut json = false;
    let mut budget = 12_000;
    let mut max_files = 8;
    let mut paths = Vec::new();
    let mut terms = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--project" => {
                project = PathBuf::from(args.next().ok_or("--project needs a directory")?)
            }
            "--report" => {
                if report_path.is_some() {
                    return Err("--report may be given once".into());
                }
                report_path = Some(args.next().ok_or("--report needs a file, or - for stdin")?);
            }
            "--message" => {
                if message.is_some() {
                    return Err("--message may be given once".into());
                }
                message = Some(args.next().ok_or("--message needs text")?);
            }
            "--json" => json = true,
            "--budget" => {
                budget = args
                    .next()
                    .ok_or("--budget needs a byte count")?
                    .parse::<usize>()
                    .map_err(|_| "--budget must be a nonnegative integer")?;
                if budget > 65_536 {
                    return Err("--budget must not exceed 65536".into());
                }
            }
            "--max-files" => {
                max_files = args
                    .next()
                    .ok_or("--max-files needs a count")?
                    .parse::<usize>()
                    .map_err(|_| "--max-files must be between 1 and 16")?;
                if !(1..=16).contains(&max_files) {
                    return Err("--max-files must be between 1 and 16".into());
                }
            }
            "--path" => paths.push(args.next().ok_or("--path needs a file or directory")?),
            "--" => {
                terms.extend(args);
                break;
            }
            _ if arg.starts_with('-') => return Err(format!("unknown diagnose option `{arg}`")),
            _ => terms.push(arg),
        }
    }
    if report_path.is_some() && message.is_some() {
        return Err("use either --report or --message".into());
    }
    let report = if let Some(path) = report_path {
        let mut bytes = Vec::new();
        if path == "-" {
            std::io::stdin()
                .lock()
                .take(65_537)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("cannot read report: {e}"))?;
        } else {
            let metadata =
                std::fs::metadata(&path).map_err(|e| format!("cannot read report: {e}"))?;
            if !metadata.is_file() || metadata.len() > 65_536 {
                return Err("report must be a regular file of at most 65536 bytes".into());
            }
            std::fs::File::open(&path)
                .map_err(|e| format!("cannot open report: {e}"))?
                .take(65_537)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("cannot read report: {e}"))?;
        }
        if bytes.len() > 65_536 {
            return Err("report exceeds 65536 bytes".into());
        }
        String::from_utf8(bytes).map_err(|_| "report must be UTF-8")?
    } else {
        message.unwrap_or_default()
    };
    Ok((
        project,
        diagnose::Request {
            report,
            terms,
            paths,
            budget,
            max_files,
        },
        json,
    ))
}

fn diagnose_command(args: Vec<String>) -> ExitCode {
    let (project, request, json) = match diagnose_options(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("kiro diagnose: {error}");
            return ExitCode::from(2);
        }
    };
    let report = diagnose::diagnose_report(&project, &request);
    if json {
        println!("{}", report.structured);
    } else {
        let _ = std::io::stdout().write_all(report.text.as_bytes());
    }
    if report.status == scout::ScoutStatus::Complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let command = args.remove(0);
    match command.as_str() {
        "run" => run(args),
        "diagnose" => diagnose_command(args),
        "scout" => {
            let project = project_dir(&mut args);
            if args.is_empty() {
                eprintln!("kiro scout: give at least one term");
                return ExitCode::from(2);
            }
            let request = Request {
                terms: args,
                paths: Vec::new(),
                budget: 12_000,
            };
            match scout::scout_result(&project, &request) {
                Ok(answer) => {
                    println!("{answer}");
                    ExitCode::SUCCESS
                }
                Err(answer) => {
                    println!("{answer}");
                    ExitCode::from(1)
                }
            }
        }
        "mcp" => {
            let selected = match take_option(&mut args, "--tools") {
                None => kiro::mcp::ToolSet::ALL,
                Some(names) => match kiro::mcp::ToolSet::parse(&names) {
                    Ok(selected) => selected,
                    Err(error) => {
                        eprintln!("kiro mcp: {error}");
                        return ExitCode::from(2);
                    }
                },
            };
            let project = project_dir(&mut args);
            if !args.is_empty() {
                eprintln!("kiro mcp: unexpected arguments (try: kiro --help)");
                return ExitCode::from(2);
            }
            match kiro::mcp::serve_with_tools(&project, selected) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("kiro mcp: {error}");
                    ExitCode::from(1)
                }
            }
        }
        "-V" | "--version" | "version" => {
            println!("kiro {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "-h" | "--help" | "help" => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("kiro: unknown command `{other}` (try: kiro --help)");
            ExitCode::from(2)
        }
    }
}
