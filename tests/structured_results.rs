//! Contract checks against public MCP responses, including adversarial file contents.
use kiro::engine::Session;
use kiro::mcp::respond;
use serde_json::{json, Value};
use std::path::PathBuf;

struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("kiro-structured-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "alpha\nbeta\nalpha again\n").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "alpha sub\n").unwrap();
        Self(dir)
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
fn call(session: &mut Session, name: &str, arguments: Value) -> Value {
    respond(
        session,
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":name,"arguments":arguments}}),
    )
    .unwrap()["result"]
        .clone()
}

#[test]
fn structured_stdout_and_offsets_are_independent_of_grouped_presentation() {
    let fixture = Fixture::new("offsets");
    let name = "colon:2:-file.txt";
    std::fs::write(fixture.0.join(name), "alpha é\nbeta\nalpha again\n").unwrap();
    let command = format!("grep -n -C1 alpha '{name}' a.txt");
    let grouped = call(&mut fixture.session(), "run", json!({"command":command}));
    let exact = call(
        &mut fixture.session(),
        "run",
        json!({"command":command,"exact":true}),
    );
    assert_eq!(grouped["structuredContent"], exact["structuredContent"]);
    assert_ne!(grouped["content"], exact["content"]);
    let data = &grouped["structuredContent"];
    assert_eq!(data["schema_version"], 1);
    assert_eq!(data["status"], "complete");
    let output = &data["commands"][0];
    let stdout = output["stdout"].as_str().unwrap().as_bytes();
    let records = output["search"]["records"].as_array().unwrap();
    assert_eq!(records.len(), 6);
    assert_eq!(records[0]["path"], name);
    assert_eq!(records[1]["kind"], "context");
    for record in records {
        let start = record["text_start"].as_u64().unwrap() as usize;
        let end = record["text_end"].as_u64().unwrap() as usize;
        assert!(matches!(
            &stdout[start..end],
            b"alpha \xc3\xa9" | b"beta" | b"alpha again" | b"alpha"
        ));
        assert_eq!(
            stdout[record["output_end"].as_u64().unwrap() as usize - 1],
            b'\n'
        );
    }
}

#[test]
fn file_contents_that_look_like_grep_are_never_regrouped() {
    let fixture = Fixture::new("arbitrary");
    let contents = format!(
        "a.txt:1:pretend\na.txt:2:pretend\na{}\n",
        ":1:".repeat(6000)
    );
    std::fs::write(fixture.0.join("data.txt"), &contents).unwrap();
    for command in [
        "cat data.txt",
        "cat data.txt | head -3",
        "grep -n alpha a.txt; cat data.txt",
    ] {
        let grouped = call(&mut fixture.session(), "run", json!({"command":command}));
        let exact = call(
            &mut fixture.session(),
            "run",
            json!({"command":command,"exact":true}),
        );
        assert_eq!(grouped, exact, "{command}");
        assert!(grouped["structuredContent"]["commands"][0]["search"].is_null());
    }
    let answer = call(
        &mut fixture.session(),
        "run",
        json!({"command":"cat data.txt"}),
    );
    assert_eq!(answer["content"][0]["text"], contents);
}

#[test]
fn batches_retain_refusals_and_each_commands_directory_and_status() {
    let fixture = Fixture::new("batch-status");
    let mut session = fixture.session();
    let answer = call(
        &mut session,
        "run",
        json!({"commands":["cd sub","rm b.txt","grep -n alpha b.txt","cat missing"]}),
    );
    let data = &answer["structuredContent"];
    assert_eq!(answer["isError"], true);
    assert_eq!(data["status"], "partial");
    let commands = data["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 4);
    assert_eq!(commands[1]["status"], "refused");
    assert!(commands[1]["exit_code"].is_null());
    assert!(commands[1]["reason"].as_str().unwrap().contains("rm"));
    assert_eq!(commands[1]["cwd_before"], commands[1]["cwd_after"]);
    assert_eq!(commands[2]["stdout"], "1:alpha sub\n");
    assert_eq!(commands[2]["search"]["records"][0]["path"], "b.txt");
    assert_eq!(commands[3]["exit_code"], 1);
    assert_eq!(commands[3]["stdout"], "");
    assert!(commands[3]["stderr"]
        .as_str()
        .unwrap()
        .contains("No such file"));
    assert_eq!(session.dir, fixture.0.join("sub"));
    assert!(session.dir.join("b.txt").exists());
    let answer = call(&mut session, "run", json!({"command":"grep absent b.txt"}));
    assert_eq!(answer["isError"], false);
    assert_eq!(answer["structuredContent"]["status"], "complete");
    assert_eq!(answer["structuredContent"]["commands"][0]["exit_code"], 1);
}

#[test]
fn invalid_argument_types_fail_before_any_command_is_executed() {
    let fixture = Fixture::new("validation");
    for arguments in [
        json!({"commands":["cd sub",42]}),
        json!({"commands":"echo x"}),
        json!({"command":42}),
        json!({"command":"cd sub","exact":"yes"}),
        json!([]),
    ] {
        let mut session = fixture.session();
        let answer = call(&mut session, "run", arguments);
        assert_eq!(answer["isError"], true);
        assert_eq!(answer["structuredContent"]["status"], "error");
        assert_eq!(answer["structuredContent"]["commands"], json!([]));
        assert_eq!(session.dir, fixture.0);
    }
    for arguments in [
        json!({"terms":[42]}),
        json!({"terms":["alpha"],"budget":-1}),
        json!({"terms":["alpha"],"paths":"sub"}),
    ] {
        let answer = call(&mut fixture.session(), "scout", arguments);
        assert_eq!(answer["isError"], true);
        assert_eq!(answer["structuredContent"]["status"], "error");
    }
}

#[test]
fn scout_metadata_and_error_reason_survive_zero_text_budget() {
    let fixture = Fixture::new("scout");
    let answer = call(
        &mut fixture.session(),
        "scout",
        json!({"terms":["alpha"],"budget":0}),
    );
    let data = &answer["structuredContent"];
    assert_eq!(answer["content"][0]["text"], "");
    assert_eq!(answer["isError"], false);
    assert_eq!(data["status"], "complete");
    assert_eq!(data["counts"]["matching_files"], 2);
    assert_eq!(data["counts"]["matching_lines"], 3);
    assert_eq!(data["matches"].as_array().unwrap().len(), 3);
    assert_eq!(data["display_truncated"], true);
    let answer = call(
        &mut fixture.session(),
        "scout",
        json!({"terms":[" "],"budget":0}),
    );
    assert_eq!(answer["content"][0]["text"], "");
    assert_eq!(answer["isError"], true);
    assert_eq!(answer["structuredContent"]["status"], "error");
    assert!(!answer["structuredContent"]["reason"]
        .as_str()
        .unwrap()
        .is_empty());
}

#[test]
fn encoded_size_refusal_rolls_back_directory_changes() {
    let fixture = Fixture::new("encoded-size");
    std::fs::write(fixture.0.join("sub/control"), vec![1u8; 1024 * 1024]).unwrap();
    let mut session = fixture.session();
    let answer = call(
        &mut session,
        "run",
        json!({"command":"cd sub; cat control"}),
    );
    assert_eq!(answer["isError"], true);
    assert_eq!(answer["structuredContent"]["status"], "error");
    assert!(answer["structuredContent"]["reason"]
        .as_str()
        .unwrap()
        .contains("8 MiB"));
    assert_eq!(session.dir, fixture.0);
}

#[test]
fn accumulated_search_metadata_is_bounded_for_batches() {
    let fixture = Fixture::new("batch-metadata");
    std::fs::write(fixture.0.join("sub/many"), "alpha\n".repeat(3000)).unwrap();
    let mut commands = vec!["cd sub"];
    commands.extend(std::iter::repeat_n("grep -n alpha many", 40));
    let mut session = fixture.session();
    let answer = call(&mut session, "run", json!({"commands":commands}));
    assert_eq!(answer["isError"], true);
    assert_eq!(answer["structuredContent"]["status"], "error");
    assert!(answer["structuredContent"]["reason"]
        .as_str()
        .unwrap()
        .contains("8 MiB"));
    assert_eq!(session.dir, fixture.0);
}

#[test]
fn protocol_negotiation_does_not_claim_unknown_versions() {
    let fixture = Fixture::new("protocol");
    let mut session = fixture.session();
    let answer = respond(
        &mut session,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":"2099-01-01"}}),
    )
    .unwrap();
    assert_eq!(answer["result"]["protocolVersion"], "2025-11-25");
    let list = respond(
        &mut session,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .unwrap();
    for tool in list["result"]["tools"].as_array().unwrap() {
        assert_eq!(tool["outputSchema"]["type"], "object");
        assert_eq!(
            tool["outputSchema"]["properties"]["schema_version"]["const"],
            if tool["name"] == "scout" { 2 } else { 1 }
        );
        assert_eq!(
            tool["outputSchema"]["properties"]["tool"]["const"],
            tool["name"]
        );
    }
}
