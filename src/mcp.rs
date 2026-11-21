//! MCP server: the engine and the scout, served to an AI agent.
//!
//! The transport is standard input/output with one JSON-RPC message per line. The server
//! process lives for the whole session, so the working directory is remembered between
//! calls and nothing has to be started again for each command.
//!
//! `run` never falls back to a shell. `scout` lists files with the system Git (hooks and
//! filesystem-monitor commands disabled), then reads them. This is not an OS sandbox.

use crate::engine::{self, Session};
use crate::limits;
use crate::scout;
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::path::Path;

const DEFAULT_SCOUT_BUDGET: usize = 12_000;

const INSTRUCTIONS: &str = "To find where something lives in the code, start with `scout`: one call with all your terms returns matching files and context. For reading and searching, prefer `run` with familiar commands and batch independent reads in `commands`. Unsupported syntax, non-regular files and requests exceeding resource limits are refused; use the regular shell for those. `run` starts no shell or program. `scout` uses system Git with hooks and filesystem-monitor commands disabled. These tools are designed for reading, but do not provide an OS sandbox or confine access to the project directory.";

fn tools() -> Value {
    let mut tools = json!([
        {
            "name": "run",
            "description": "Fast, read-only terminal. Runs read/search commands exactly as written (cat, sed -n 'A,Bp', head, tail, wc, cut, ls, grep with its usual options, pipelines, `;`/`&&`/`||` lists, `cd dir`) without starting a shell or a program. Each call is one full round trip, so batch: put EVERYTHING you need next in `commands` and get all the answers at once. Known grep line listings may be grouped by file (the path once, then `line:text`). Structured results retain original stdout, stderr, exit codes and bounded search records. The working directory is remembered between calls. Commands it does not cover are refused (nothing is executed): use the regular shell for those.",
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "commands": { "type": "array", "maxItems": 128, "items": { "type": "string", "maxLength": 65536 }, "description": "The commands to run, in order; all outputs come back together. Prefer this, with everything you need." },
                    "command": { "type": "string", "description": "A single command, when there is really only one thing to ask." },
                    "exact": { "type": "boolean", "description": "Disable grouping in the human-readable text. Structured stdout and stderr are always separate and ungrouped; search offsets refer to structured stdout." }
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
                    "budget": { "type": "integer", "description": "Maximum answer size in UTF-8 bytes (default 12000, capped at 65536)." }
                },
                "required": ["terms"]
            }
        }
    ]);
    for tool in tools.as_array_mut().unwrap() {
        tool["outputSchema"] = output_schema(tool["name"].as_str().unwrap());
    }
    tools
}

/// Versioned contracts shared by success, refusals and input errors.
fn output_schema(name: &str) -> Value {
    let nullable_string = json!({"type":["string","null"]});
    let count = json!({"type":"integer","minimum":0});
    let mut properties = json!({
        "schema_version":{"const":1}, "tool":{"const":name},
        "status":{"enum":["complete","partial","refused","error"]},
        "reason":nullable_string, "cwd":{"type":"string"},
    });
    let mut required = vec!["schema_version", "tool", "status", "reason", "cwd"];
    if name == "run" {
        let record = json!({
            "type":"object","additionalProperties":false,
            "required":["path","line_number","kind","output_start","text_start","text_end","output_end"],
            "properties":{
                "path":nullable_string,"line_number":{"type":"integer","minimum":1},
                "kind":{"enum":["match","context"]},
                "output_start":count,"text_start":count,"text_end":count,"output_end":count,
            }
        });
        properties["commands"] = json!({
            "type":"array","maxItems":128,"items":{
                "type":"object","additionalProperties":false,
                "required":["command","status","reason","stdout","stderr","exit_code","cwd_before","cwd_after","search"],
                "properties":{
                    "command":{"type":"string"},"status":{"enum":["complete","refused"]},
                    "reason":nullable_string,"stdout":{"type":"string"},"stderr":{"type":"string"},
                    "exit_code":{"type":["integer","null"]},
                    "cwd_before":{"type":"string"},"cwd_after":{"type":"string"},
                    "search":{"anyOf":[{"type":"null"},{
                        "type":"object","additionalProperties":false,
                        "required":["metadata_truncated","records"],
                        "properties":{"metadata_truncated":{"type":"boolean"},
                            "records":{"type":"array","maxItems":4096,"items":record}},
                    }]},
                }
            }
        });
        required.push("commands");
    } else {
        properties["terms"] = json!({"type":"array","items":{"type":"string"},"maxItems":60});
        properties["counts"] = json!({
            "type":"object","additionalProperties":false,
            "required":["candidates","read","skipped","errors","matching_files","matching_lines"],
            "properties":{"candidates":count,"read":count,"skipped":count,"errors":count,"matching_files":count,"matching_lines":count},
        });
        properties["matches"] = json!({
            "type":"array","maxItems":4096,"items":{
                "type":"object","additionalProperties":false,"required":["path","line_number","term_indices"],
                "properties":{"path":{"type":"string"},"line_number":{"type":"integer","minimum":1},
                    "term_indices":{"type":"array","maxItems":60,"items":count}},
            }
        });
        properties["metadata_truncated"] = json!({"type":"boolean"});
        properties["display_truncated"] = json!({"type":"boolean"});
        required.extend([
            "terms",
            "counts",
            "matches",
            "metadata_truncated",
            "display_truncated",
        ]);
    }
    json!({"type":"object","additionalProperties":false,"required":required,"properties":properties})
}

/// Group only records emitted by grep itself. Arbitrary stdout is never interpreted as
/// a listing, and no filesystem probes or prefix caches are needed.
fn group_search_output(text: &str, listing: &crate::grep::SearchListing) -> Option<String> {
    if listing.truncated || listing.records.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut current = None;
    let mut end = 0;
    let mut grouped = false;
    for record in &listing.records {
        let path = record.path.as_deref()?;
        if path.contains(['\n', '\r']) {
            return None;
        }
        let gap = text.get(end..record.output_start)?;
        if !gap.is_empty() && gap != "--\n" {
            return None;
        }
        out.push_str(gap);
        let separator = match record.kind {
            crate::grep::SearchKind::Match => ':',
            crate::grep::SearchKind::Context => '-',
        };
        let prefix = format!("{path}{separator}{}{separator}", record.line_number);
        if text.get(record.output_start..record.text_start)? != prefix {
            return None;
        }
        if current == Some(path) {
            grouped = true;
        } else {
            out.push_str(path);
            out.push_str(":\n");
            current = Some(path);
        }
        // Preserve the original line-number separator, contents, and final newline.
        out.push_str(text.get(record.output_start + path.len() + 1..record.output_end)?);
        end = record.output_end;
    }
    if end != text.len() || !grouped || out.len() >= text.len() {
        return None;
    }
    Some(out)
}

struct ToolAnswer {
    text: String,
    structured: Value,
    is_error: bool,
}

fn search_json(search: &crate::grep::SearchListing) -> Value {
    json!({
        "metadata_truncated": search.truncated,
        "records": search.records.iter().map(|record| json!({
            "path": record.path,
            "line_number": record.line_number,
            "kind": match record.kind {
                crate::grep::SearchKind::Match => "match",
                crate::grep::SearchKind::Context => "context",
            },
            "output_start": record.output_start,
            "text_start": record.text_start,
            "text_end": record.text_end,
            "output_end": record.output_end,
        })).collect::<Vec<_>>()
    })
}

/// Execution and presentation are separate: stdout offsets always refer to the ungrouped
/// stdout field. Commit cd only after the command is representable in this transport.
fn run_one(command: &str, session: &mut Session, exact: bool) -> (String, Value, bool) {
    let before = session.dir.clone();
    let mut pending = Session {
        dir: before.clone(),
    };
    let result = engine::run_native(command, &mut pending).and_then(|reply| {
        let stdout = String::from_utf8(reply.stdout)
            .map_err(|_| "non-UTF-8 output cannot be returned as MCP text".to_string())?;
        let mut text = if exact {
            None
        } else {
            reply
                .search
                .as_ref()
                .and_then(|listing| group_search_output(&stdout, listing))
        }
        .unwrap_or_else(|| stdout.clone());
        limits::append_error(&mut text, &reply.stderr)?;
        if reply.code != 0 {
            if !text.is_empty() && !text.ends_with('\n') {
                limits::append_error(&mut text, "\n")?;
            }
            limits::append_error(&mut text, &format!("[exit code {}]", reply.code))?;
        }
        let data = json!({
            "command": command, "status": "complete", "reason": null,
            "stdout": stdout, "stderr": reply.stderr, "exit_code": reply.code,
            "cwd_before": before.to_string_lossy(), "cwd_after": pending.dir.to_string_lossy(),
            "search": reply.search.as_ref().map(search_json),
        });
        Ok((text, data))
    });
    match result {
        Ok((text, data)) => {
            session.dir = pending.dir;
            (text, data, false)
        }
        Err(reason) => {
            let text = format!("[not run: the fast terminal does not cover this ({reason}). Use the regular shell for it.]");
            let data = json!({
                "command": command, "status": "refused", "reason": reason,
                "stdout": "", "stderr": "", "exit_code": null,
                "cwd_before": before.to_string_lossy(), "cwd_after": before.to_string_lossy(), "search": null,
            });
            (text, data, true)
        }
    }
}

fn strings(arguments: &Value, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = arguments.get(key) else {
        return Ok(Vec::new());
    };
    let list = value
        .as_array()
        .ok_or_else(|| format!("`{key}` must be an array of strings"))?;
    list.iter()
        .map(|value| {
            value
                .as_str()
                .map(String::from)
                .ok_or_else(|| format!("`{key}` must contain only strings"))
        })
        .collect()
}

fn empty_report(name: &str, dir: &Path, reason: &str) -> Value {
    let mut result = json!({
        "schema_version": 1, "tool": name, "status": "error", "reason": reason, "cwd": dir.to_string_lossy(),
    });
    match name {
        "run" => result["commands"] = json!([]),
        "scout" => {
            result["terms"] = json!([]);
            result["matches"] = json!([]);
            result["counts"] = json!({"candidates":0,"read":0,"skipped":0,"errors":0,"matching_files":0,"matching_lines":0});
            result["metadata_truncated"] = json!(false);
            result["display_truncated"] = json!(false);
        }
        _ => {}
    }
    result
}

fn call(session: &mut Session, name: &str, arguments: &Value) -> Result<ToolAnswer, String> {
    if !arguments.is_object() {
        return Err("arguments must be an object".to_string());
    }
    match name {
        "run" => {
            let mut commands = Vec::new();
            if let Some(single) = arguments.get("command") {
                commands.push(
                    single
                        .as_str()
                        .ok_or("`command` must be a string")?
                        .to_string(),
                );
            }
            commands.extend(strings(arguments, "commands")?);
            if commands.is_empty() {
                return Err("give `command` or `commands`".to_string());
            }
            if commands.len() > limits::MAX_COMMANDS {
                return Err("at most 128 commands per call".to_string());
            }
            let exact = match arguments.get("exact") {
                None => false,
                Some(value) => value.as_bool().ok_or("`exact` must be a boolean")?,
            };
            let mut text = String::new();
            let mut reports = Vec::new();
            let mut refused = 0;
            let mut encoded = EncodedCounter(0);
            let mut pending = Session {
                dir: session.dir.clone(),
            };
            for command in &commands {
                let (output, report, failed) = run_one(command, &mut pending, exact);
                let previous_len = text.len();
                if commands.len() > 1 {
                    limits::append_error(&mut text, &format!("$ {command}\n"))?;
                }
                limits::append_error(&mut text, &output)?;
                if commands.len() > 1 && !text.ends_with('\n') {
                    limits::append_error(&mut text, "\n")?;
                }
                // Account while accumulating, not after retaining up to 128 metadata
                // arrays. A refusal drops pending state and every accumulated result.
                serde_json::to_writer(&mut encoded, &report).map_err(|e| e.to_string())?;
                serde_json::to_writer(&mut encoded, &text[previous_len..])
                    .map_err(|e| e.to_string())?;
                refused += usize::from(failed);
                reports.push(report);
            }
            let status = if refused == 0 {
                "complete"
            } else if refused == commands.len() {
                "refused"
            } else {
                "partial"
            };
            let structured = json!({
                "schema_version":1,"tool":"run","status":status,"reason":null,
                "cwd":pending.dir.to_string_lossy(),"commands":reports,
            });
            let answer = ToolAnswer {
                text,
                structured,
                is_error: refused > 0,
            };
            check_answer_size(&answer)?;
            session.dir = pending.dir;
            Ok(answer)
        }
        "scout" => {
            let terms = strings(arguments, "terms")?;
            if terms.is_empty() {
                return Err("give `terms`".to_string());
            }
            let budget = match arguments.get("budget") {
                None => DEFAULT_SCOUT_BUDGET,
                Some(value) => value
                    .as_u64()
                    .ok_or("`budget` must be a nonnegative integer")?
                    .min(65_536) as usize,
            };
            let request = scout::Request {
                terms,
                paths: strings(arguments, "paths")?,
                budget,
            };
            let report = scout::scout_report(&session.dir, &request);
            let structured = json!({
                "schema_version":1,"tool":"scout","status":report.status.as_str(),
                "reason":report.reason,"cwd":session.dir.to_string_lossy(),"terms":report.terms,
                "counts":{
                    "candidates":report.counts.candidates,"read":report.counts.read,
                    "skipped":report.counts.skipped,"errors":report.counts.errors,
                    "matching_files":report.counts.matching_files,"matching_lines":report.counts.matching_lines,
                },
                "matches":report.matches.iter().map(|hit| json!({
                    "path":hit.path,"line_number":hit.line_number,"term_indices":hit.term_indices,
                })).collect::<Vec<_>>(),
                "metadata_truncated":report.metadata_truncated,"display_truncated":report.display_truncated,
            });
            let answer = ToolAnswer {
                text: report.text,
                structured,
                is_error: report.status != scout::ScoutStatus::Complete,
            };
            check_answer_size(&answer)?;
            Ok(answer)
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

// Bound the encoded content plus metadata too, including JSON escape expansion. Count
// through a writer instead of allocating a second, potentially large serialized copy.
const MAX_ENCODED_ANSWER: usize = 8 * 1024 * 1024;
struct EncodedCounter(usize);
impl Write for EncodedCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_ENCODED_ANSWER {
            return Err(std::io::Error::other("MCP response exceeds 8 MiB encoded"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn check_answer_size(answer: &ToolAnswer) -> Result<(), String> {
    let mut counter = EncodedCounter(0);
    serde_json::to_writer(&mut counter, &answer.text).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut counter, &answer.structured).map_err(|e| e.to_string())
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
                .filter(|version| {
                    matches!(
                        *version,
                        "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25"
                    )
                })
                .unwrap_or("2025-11-25");
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
            let answer = match call(session, name, &arguments) {
                Ok(answer) => answer,
                Err(error) => ToolAnswer {
                    structured: empty_report(name, &session.dir, &error),
                    text: error,
                    is_error: true,
                },
            };
            json!({ "content": [{ "type": "text", "text": answer.text }],
                "structuredContent": answer.structured, "isError": answer.is_error })
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
    let project = if project.is_absolute() {
        project.to_path_buf()
    } else {
        engine::current_dir()?.join(project)
    };
    let mut session = Session {
        dir: engine::logical_directory(&project).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "project directory cannot be resolved",
            )
        })?,
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut stdin = stdin.lock();
    loop {
        let mut line = Vec::new();
        (&mut stdin)
            .take(limits::MAX_MESSAGE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if line.is_empty() {
            break;
        }
        if line.len() > limits::MAX_MESSAGE_BYTES {
            writeln!(
                stdout,
                "{}",
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"message exceeds 1 MiB"}})
            )?;
            stdout.flush()?;
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let answer = match serde_json::from_slice::<Value>(&line) {
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
