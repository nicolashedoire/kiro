//! `kiro`: the command-line entry point.
//!
//! - `kiro run COMMAND` runs a command through the engine (and tells how, with `--stats`).
//! - `kiro mcp` serves the engine and the scout to an AI agent over MCP.
//! - `kiro scout TERM...` prints the map of an area of the code.

use kiro::engine::{self, Session};
use kiro::scout::{self, Request};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

const HELP: &str = "\
kiro - a terminal engine for AI coding agents

USAGE:
    kiro run [--stats] COMMAND      run a command through the engine
    kiro scout [--project DIR] TERM...
                                    map where the terms live in the code
    kiro mcp [--project DIR]        serve the engine to an AI agent (MCP, stdio)
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
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

fn run(mut args: Vec<String>) -> ExitCode {
    let stats = take_flag(&mut args, "--stats");
    let command = args.join(" ");
    if command.trim().is_empty() {
        eprintln!("kiro run: the command is missing");
        return ExitCode::from(2);
    }
    let mut session = Session {
        dir: std::env::current_dir().unwrap_or_default(),
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

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let command = args.remove(0);
    match command.as_str() {
        "run" => run(args),
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
            println!("{}", scout::scout(&project, &request));
            ExitCode::SUCCESS
        }
        "mcp" => {
            let project = project_dir(&mut args);
            match kiro::mcp::serve(&project) {
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
