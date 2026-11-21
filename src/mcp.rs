//! MCP server: the engine and the scout, served to an AI agent.
//!
//! The transport is standard input/output with one JSON-RPC message per line. The server
//! process lives for the whole session, so the working directory is remembered between
//! calls and nothing has to be started again for each command.
//!
//! Both tools are read-only by construction: `run` refuses any command the engine does not
//! cover instead of passing it to a shell, and `scout` only reads files. One permission
//! therefore covers every call.

use crate::engine::{self, Session};
use crate::scout;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;

const DEFAULT_SCOUT_BUDGET: usize = 12_000;

const INSTRUCTIONS: &str = "To find where something lives in the code, START with `scout`: one call with all your terms returns the files, the matching lines and the functions they sit in. For reading and searching files (cat, sed -n, head, tail, wc, cut, ls, grep, pipelines and `;`/`&&` lists of those), ALWAYS call `run` instead of the regular shell or the file-reading tools: same commands, no shell started. Every call to any tool is a full round trip that re-reads the whole conversation, so the number of calls is what costs: before calling, think about everything you will need next and send it all in ONE call through `commands`. `run` and `scout` cannot modify anything; use the regular shell only for what `run` refuses.";

fn tools() -> Value {
    json!([
        {
            "name": "run",
            "description": "Fast, read-only terminal. Runs read/search commands exactly as written (cat, sed -n 'A,Bp', head, tail, wc, cut, ls, grep with its usual options, pipelines, `;`/`&&`/`||` lists, `cd dir`) without starting a shell or a program. Each call is one full round trip, so batch: put EVERYTHING you need next in `commands` and get all the answers at once. Search listings come back grouped by file (the path once, then `line:text`). The working directory is remembered between calls. Commands it does not cover are refused (nothing is executed): use the regular shell for those.",
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "commands": { "type": "array", "items": { "type": "string" }, "description": "The commands to run, in order; all outputs come back together. Prefer this, with everything you need." },
                    "command": { "type": "string", "description": "A single command, when there is really only one thing to ask." },
                    "exact": { "type": "boolean", "description": "Return the output byte for byte, without grouping search listings by file." }
                }
            }
        },
        {
            "name": "scout",
            "description": "START HERE when you need to find where something lives in the code. One call replaces a dozen searches: give the words, phrases and identifiers you care about (domain words in every language the code may use, error messages, function or constant names you guess) and get back the files that mention them, ranked, with the matching lines and the function each line sits in. Then read only what you still need, with `run`.",
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terms": { "type": "array", "items": { "type": "string" }, "description": "Plain-text terms, case-insensitive: words, phrases, identifiers. Give many (5 to 15)." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional directories or files to restrict the search to." },
                    "budget": { "type": "integer", "description": "Maximum size of the answer in characters (default 12000)." }
                },
                "required": ["terms"]
            }
        }
    ])
}

/// Groups a grep-style listing by file: the path is written once, each line keeps `12:text`.
///
/// Applied only when every line starts with the path of an existing file followed by a line
/// number, so nothing is lost and nothing else is touched. Returns `None` when the text is
/// not such a listing or when grouping would not make it shorter.
pub fn group_by_file(text: &str, dir: &Path) -> Option<String> {
    let mut known: HashMap<String, bool> = HashMap::new();
    let mut is_file = |path: &str| -> bool {
        *known
            .entry(path.to_string())
            .or_insert_with(|| dir.join(path).is_file())
    };
    // Finds where `path` ends in `path:12:text` or `path-12-text`.
    let mut path_end = |line: &str| -> Option<usize> {
        let bytes = line.as_bytes();
        for i in 1..bytes.len() {
            if bytes[i] != b':' && bytes[i] != b'-' {
                continue;
            }
            let separator = bytes[i];
            let digits = bytes[i + 1..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            let after = i + 1 + digits;
            if digits > 0 && bytes.get(after) == Some(&separator) && is_file(&line[..i]) {
                return Some(i);
            }
        }
        None
    };
    let mut out = String::with_capacity(text.len());
    let mut current: Option<&str> = None;
    let mut grouped = 0usize;
    for line in text.lines() {
        if line == "--" {
            out.push_str("--\n");
            continue;
        }
        let end = path_end(line)?;
        let path = &line[..end];
        if current == Some(path) {
            grouped += 1;
        } else {
            out.push_str(path);
            out.push_str(":\n");
            current = Some(path);
        }
        out.push_str(&line[end + 1..]);
        out.push('\n');
    }
    if grouped == 0 || out.len() >= text.len() {
        return None;
    }
    Some(out)
}

/// Runs one command inside the engine and formats the answer for the agent.
/// Returns the text and whether the command was refused.
fn run_one(command: &str, session: &mut Session, exact: bool) -> (String, bool) {
    match engine::run_native(command, session) {
        Ok(reply) => {
            let mut text = String::from_utf8_lossy(&reply.stdout).into_owned();
            if !exact {
                if let Some(grouped) = group_by_file(&text, &session.dir) {
                    text = grouped;
                }
            }
            text.push_str(&reply.stderr);
            if reply.code != 0 {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&format!("[exit code {}]", reply.code));
            }
            (text, false)
        }
        Err(reason) => (
            format!(
                "[not run: the fast terminal does not cover this ({reason}). Use the regular shell for it.]"
            ),
            true,
        ),
    }
}

fn strings(arguments: &Value, key: &str) -> Vec<String> {
    arguments
        .get(key)
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn call(session: &mut Session, name: &str, arguments: &Value) -> Result<String, String> {
    match name {
        "run" => {
            let mut commands: Vec<String> = Vec::new();
            if let Some(single) = arguments.get("command").and_then(Value::as_str) {
                commands.push(single.to_string());
            }
            commands.extend(strings(arguments, "commands"));
            if commands.is_empty() {
                return Err("give `command` or `commands`".to_string());
            }
            let exact = arguments
                .get("exact")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let [only] = commands.as_slice() {
                let (text, refused) = run_one(only, session, exact);
                return if refused { Err(text) } else { Ok(text) };
            }
            let mut out = String::new();
            for command in &commands {
                let (text, _) = run_one(command, session, exact);
                out.push_str(&format!("$ {command}\n{text}"));
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            Ok(out)
        }
        "scout" => {
            let terms = strings(arguments, "terms");
            if terms.is_empty() {
                return Err("give `terms`".to_string());
            }
            let budget = arguments
                .get("budget")
                .and_then(Value::as_u64)
                .map_or(DEFAULT_SCOUT_BUDGET, |b| b as usize);
            let request = scout::Request {
                terms,
                paths: strings(arguments, "paths"),
                budget,
            };
            Ok(scout::scout(&session.dir, &request))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Answers one JSON-RPC message. Returns `None` for a notification (no answer expected).
pub fn respond(session: &mut Session, message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05");
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "kiro", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            match call(session, name, &arguments) {
                Ok(text) => {
                    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
                }
                Err(error) => {
                    json!({ "content": [{ "type": "text", "text": error }], "isError": true })
                }
            }
        }
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("unknown method: {method}") }
            }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Server loop: reads messages on standard input, answers on standard output.
pub fn serve(project: &Path) -> std::io::Result<()> {
    let mut session = Session {
        dir: project
            .canonicalize()
            .unwrap_or_else(|_| project.to_path_buf()),
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(message) => respond(&mut session, &message),
            Err(_) => Some(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": "unreadable message" }
            })),
        };
        if let Some(answer) = answer {
            writeln!(stdout, "{answer}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> (Session, PathBuf) {
        let dir = std::env::temp_dir().join(format!("kiro-mcp-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.txt"), "one\ntwo\nthree\n").unwrap();
        (Session { dir: dir.clone() }, dir)
    }

    fn request(session: &mut Session, id: u64, method: &str, params: Value) -> Value {
        respond(
            session,
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        )
        .unwrap()
    }

    fn run(session: &mut Session, arguments: Value) -> Value {
        request(
            session,
            1,
            "tools/call",
            json!({ "name": "run", "arguments": arguments }),
        )
    }

    fn text_of(answer: &Value) -> String {
        answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn introduces_itself_and_lists_its_tools() {
        let (mut session, dir) = fixture("init");
        let answer = request(
            &mut session,
            1,
            "initialize",
            json!({ "protocolVersion": "2025-06-18" }),
        );
        assert_eq!(answer["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(answer["result"]["serverInfo"]["name"], "kiro");
        let answer = request(&mut session, 2, "tools/list", Value::Null);
        let names: Vec<&str> = answer["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["run", "scout"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn does_not_answer_notifications() {
        let (mut session, dir) = fixture("notification");
        let message = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(respond(&mut session, &message).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_executes_inside_the_engine_and_remembers_the_directory() {
        let (mut session, dir) = fixture("run");
        let answer = run(&mut session, json!({ "command": "grep -n t src/a.txt" }));
        assert_eq!(answer["result"]["isError"], false);
        assert_eq!(text_of(&answer), "2:two\n3:three\n");
        run(&mut session, json!({ "command": "cd src" }));
        let answer = run(&mut session, json!({ "command": "wc -l a.txt" }));
        assert_eq!(text_of(&answer), "       3 a.txt\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn several_commands_come_back_in_one_answer() {
        let (mut session, dir) = fixture("batch");
        let answer = run(
            &mut session,
            json!({ "commands": ["head -1 src/a.txt", "sed -n '3p' src/a.txt"] }),
        );
        assert_eq!(
            text_of(&answer),
            "$ head -1 src/a.txt\none\n$ sed -n '3p' src/a.txt\nthree\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_listings_are_grouped_by_file_without_losing_anything() {
        let (mut session, dir) = fixture("group");
        std::fs::write(dir.join("src/b-c.txt"), "two\nthree\n").unwrap();
        let command = "grep -n t src/a.txt src/b-c.txt";
        let grouped = text_of(&run(&mut session, json!({ "command": command })));
        assert_eq!(
            grouped,
            "src/a.txt:\n2:two\n3:three\nsrc/b-c.txt:\n1:two\n2:three\n"
        );
        let exact = text_of(&run(
            &mut session,
            json!({ "command": command, "exact": true }),
        ));
        assert_eq!(
            exact,
            "src/a.txt:2:two\nsrc/a.txt:3:three\nsrc/b-c.txt:1:two\nsrc/b-c.txt:2:three\n"
        );
        // The content of a file is never regrouped.
        let content = text_of(&run(&mut session, json!({ "command": "cat src/a.txt" })));
        assert_eq!(content, "one\ntwo\nthree\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_command_that_could_modify_something_is_refused_not_run() {
        let (mut session, dir) = fixture("refuse");
        let answer = run(&mut session, json!({ "command": "rm src/a.txt" }));
        assert_eq!(answer["result"]["isError"], true);
        assert!(dir.join("src/a.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scout_is_served_as_a_tool() {
        let (mut session, dir) = fixture("scout");
        let answer = request(
            &mut session,
            1,
            "tools/call",
            json!({ "name": "scout", "arguments": { "terms": ["three"] } }),
        );
        assert_eq!(answer["result"]["isError"], false);
        assert!(text_of(&answer).contains("src/a.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_method_returns_an_error() {
        let (mut session, dir) = fixture("unknown");
        let answer = request(&mut session, 1, "nope", Value::Null);
        assert_eq!(answer["error"]["code"], -32601);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
