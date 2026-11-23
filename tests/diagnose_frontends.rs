//! The CLI and MCP preserve evidence status and expose only enabled tools.
use kiro::engine::Session;
use kiro::mcp::{respond, respond_with_tools, ToolSet};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("kiro-diagnose-front-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("src")).unwrap();
        std::fs::write(path.join("src/service.js"), "export function validateInput(value) {\n  if (!value) {\n    throw new Error('MissingWidget');\n  }\n  return value;\n}\n").unwrap();
        Self(path)
    }
    fn session(&self) -> Session {
        Session {
            dir: self.0.clone(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn call(session: &mut Session, args: Value) -> Value {
    respond(session, &json!({"jsonrpc":"2.0","id":1,"method":"tools/call", "params":{"name":"diagnose","arguments":args}})).unwrap()["result"].clone()
}

#[test]
fn mcp_restricted_toolset_does_not_advertise_or_execute_diagnose() {
    let fixture = Fixture::new("restricted");
    let selected = ToolSet::parse("run,scout").unwrap();
    let mut session = fixture.session();
    let list = respond_with_tools(
        &mut session,
        &json!({"id":1,"method":"tools/list"}),
        selected,
    )
    .unwrap();
    let names: Vec<_> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["run", "scout"]);
    let init = respond_with_tools(
        &mut session,
        &json!({"id":2,"method":"initialize"}),
        selected,
    )
    .unwrap();
    assert!(!init["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("`diagnose`"));
    let denied = respond_with_tools(&mut session, &json!({"id":3,"method":"tools/call", "params":{"name":"diagnose","arguments":{"report":"MissingWidget"}}}), selected).unwrap();
    assert_eq!(denied["result"]["isError"], true);
    assert!(denied["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("not enabled"));
    assert_eq!(session.dir, fixture.0);
    assert!(ToolSet::parse("run,typo").is_err());
    assert!(ToolSet::parse("").is_err());
}

#[test]
fn mcp_zero_display_budget_keeps_status_and_source_evidence() {
    let fixture = Fixture::new("zero");
    let result = call(
        &mut fixture.session(),
        json!({"report":"MissingWidget at src/service.js:3:5", "budget":0}),
    );
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["content"][0]["text"], "");
    let data = &result["structuredContent"];
    assert_eq!(data["tool"], "diagnose");
    assert_eq!(data["schema_version"], 1);
    assert_eq!(data["status"], "complete");
    assert!(data["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"] == "src/service.js"));
}

#[test]
fn mcp_bad_arguments_are_structured_errors_without_a_scan() {
    let fixture = Fixture::new("invalid");
    for arguments in [
        json!({"report":12}),
        json!({"report":"MissingWidget","unrecognized":true}),
        json!({"report":"MissingWidget","max_files":0}),
        json!({"report":"MissingWidget","max_files":17}),
        json!({"report":"MissingWidget","budget":-1}),
        json!({"report":"MissingWidget","budget":65537}),
        json!({"report":"MissingWidget","terms":[false]}),
    ] {
        let result = call(&mut fixture.session(), arguments);
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(result["structuredContent"]["status"], "error");
        assert_eq!(result["structuredContent"]["candidates"], json!([]));
    }
}

#[test]
fn cli_reads_a_report_and_returns_the_same_structured_evidence() {
    let fixture = Fixture::new("cli");
    let log = fixture.0.join("failure.log");
    std::fs::write(&log, "MissingWidget at src/service.js:3:5").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kiro"))
        .args(["diagnose", "--project"])
        .arg(&fixture.0)
        .arg("--report")
        .arg(&log)
        .args(["--json", "--budget", "0"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mcp = call(
        &mut fixture.session(),
        json!({"report":"MissingWidget at src/service.js:3:5","budget":0}),
    );
    // Compare the wire representations; serde_json's default float parser may round
    // a score by one ulp compared with the in-memory Value constructed by the library.
    let mcp_wire: Value =
        serde_json::from_slice(&serde_json::to_vec(&mcp["structuredContent"]).unwrap()).unwrap();
    assert_eq!(cli, mcp_wire);
    assert_eq!(
        std::fs::read(&log).unwrap(),
        b"MissingWidget at src/service.js:3:5"
    );
}

#[test]
fn cli_rejects_unbounded_or_non_text_report_input_before_inspection() {
    let fixture = Fixture::new("input");
    let log = fixture.0.join("invalid.log");
    std::fs::write(&log, [0xff, 0xfe]).unwrap();
    let invalid = Command::new(env!("CARGO_BIN_EXE_kiro"))
        .args(["diagnose", "--report"])
        .arg(&log)
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("UTF-8"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_kiro"))
        .args(["diagnose", "--report", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&vec![b'a'; 65_537])
        .unwrap();
    let too_large = child.wait_with_output().unwrap();
    assert_eq!(too_large.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&too_large.stderr).contains("65536"));
}
