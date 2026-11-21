//! Audit regressions: real filesystem and MCP subprocess behavior, not implementation copies.
#![cfg(target_os = "macos")]

use kiro::engine::{run, run_native, Session};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("kiro-audit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("~")).unwrap();
        std::fs::write(dir.join("notes.txt"), "alpha\nbeta\n").unwrap();
        std::fs::write(dir.join("~/literal.txt"), "literal tilde\n").unwrap();
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

#[test]
fn quoted_and_escaped_tildes_are_literal() {
    let fixture = Fixture::new("tilde");
    for command in [
        "cat '~/literal.txt'",
        "cat \"~/literal.txt\"",
        "cat \\~/literal.txt",
    ] {
        let actual = run_native(command, &mut fixture.session()).unwrap();
        let expected = Command::new("/bin/sh")
            .args(["-c", command])
            .current_dir(&fixture.0)
            .output()
            .unwrap();
        assert_eq!(actual.stdout, expected.stdout, "{command}");
        assert_eq!(actual.code, 0);
    }
    let mut session = fixture.session();
    assert_eq!(
        run_native("cd '~'; cat literal.txt", &mut session)
            .unwrap()
            .stdout,
        b"literal tilde\n"
    );
    let mut session = fixture.session();
    run_native("cd", &mut session).unwrap();
    assert_eq!(session.dir, PathBuf::from(std::env::var("HOME").unwrap()));
}

#[test]
fn shell_expansions_and_program_paths_are_refused() {
    let fixture = Fixture::new("syntax");
    for command in [
        "echo \"pid=$$\"",
        "echo $?",
        "echo $@",
        "echo $*",
        "echo $#",
        "echo $-",
        "echo $!",
        "echo $0",
        "echo \"$+HOME\"",
        "echo \"$[1+2]\"",
        "echo \"$=HOME\"",
        "echo \"$^HOME\"",
        "echo \"$<\"",
        "echo $'text'",
        "echo ~root",
        "/nonexistent/cat notes.txt",
        "/bin/cat notes.txt",
        "command /bin/cat notes.txt",
    ] {
        assert!(
            run_native(command, &mut fixture.session()).is_err(),
            "{command}"
        );
    }
    assert_eq!(
        run_native("echo '$$'", &mut fixture.session())
            .unwrap()
            .stdout,
        b"$$\n"
    );
}

#[test]
fn mixed_streams_are_refused_and_cli_fallback_preserves_order() {
    let fixture = Fixture::new("streams");
    let command = "cat notes.txt absent 2>&1";
    assert!(run_native(command, &mut fixture.session()).is_err());
    let actual = run(command, &mut fixture.session());
    let expected = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(&fixture.0)
        .output()
        .unwrap();
    assert!(!actual.native);
    assert_eq!(actual.stdout, expected.stdout);
    for command in [
        "echo value2>&1",
        "cat notes.txt 2>&1 2>/dev/null",
        "cat notes.txt 2>/dev/null 2>&1",
    ] {
        assert!(
            run_native(command, &mut fixture.session()).is_err(),
            "{command}"
        );
    }
    assert_eq!(
        run_native("cat notes.txt 2>&1", &mut fixture.session())
            .unwrap()
            .stdout,
        b"alpha\nbeta\n"
    );
}

#[test]
fn resource_limits_refuse_whole_results_and_rollback_directory() {
    let fixture = Fixture::new("limits");
    std::fs::File::create(fixture.0.join("too-large"))
        .unwrap()
        .set_len(8 * 1024 * 1024 + 1)
        .unwrap();
    std::fs::write(
        fixture.0.join("large-output"),
        vec![b'a'; 2 * 1024 * 1024 + 1],
    )
    .unwrap();
    for command in ["head -1 too-large", "cat large-output", "cat /dev/zero"] {
        assert!(
            run_native(command, &mut fixture.session()).is_err(),
            "{command}"
        );
    }
    let mut session = fixture.session();
    assert!(run_native("cd '~'; cat ../large-output", &mut session).is_err());
    assert_eq!(session.dir, fixture.0);
    let command = format!("echo {}", "x".repeat(65_536));
    assert!(run_native(&command, &mut fixture.session()).is_err());
    std::fs::write(fixture.0.join("fields"), ":".repeat(2 * 1024 * 1024 + 1)).unwrap();
    assert!(run_native("cut -d: -f1- fields", &mut fixture.session()).is_err());
}

/// A deadline makes a FIFO regression fail instead of hanging the test suite indefinitely.
fn mcp(fixture: &Fixture, input: Vec<u8>) -> Vec<Value> {
    mcp_with_home(fixture, input, None)
}

fn mcp_with_home(fixture: &Fixture, input: Vec<u8>, home: Option<&str>) -> Vec<Value> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kiro"));
    command
        .args(["mcp", "--project"])
        .arg(&fixture.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(home) = home {
        command.env("HOME", home);
    }
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .take(4 * 1024 * 1024)
            .read_to_end(&mut output)
            .unwrap();
        output
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(3) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("MCP blocked on a request");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    writer.join().unwrap();
    assert!(status.success());
    reader
        .join()
        .unwrap()
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

#[test]
fn mcp_refuses_fifo_and_remains_responsive() {
    let fixture = Fixture::new("fifo");
    assert!(Command::new("/usr/bin/mkfifo")
        .arg(fixture.0.join("stream"))
        .status()
        .unwrap()
        .success());
    let messages = [
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"run","arguments":{"command":"head -1 stream"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run","arguments":{"command":"echo responsive"}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run","arguments":{"commands":vec!["echo x";129]}}}),
    ];
    let input = messages
        .iter()
        .map(|message| format!("{message}\n"))
        .collect::<String>()
        .into_bytes();
    let answers = mcp(&fixture, input);
    assert_eq!(answers[0]["result"]["isError"], true);
    assert!(answers[0]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("non-regular"));
    assert_eq!(answers[1]["result"]["content"][0]["text"], "responsive\n");
    assert_eq!(answers[2]["result"]["isError"], true);
}

#[test]
fn mcp_rejects_oversized_transport_messages() {
    let fixture = Fixture::new("message");
    let mut input = vec![b' '; 1024 * 1024 + 1];
    input.push(b'\n');
    let answers = mcp(&fixture, input);
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0]["error"]["code"], -32600);
}

#[test]
fn expanded_words_are_bounded_before_execution() {
    let fixture = Fixture::new("expanded");
    let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"run","arguments":{"command":format!("echo {}", "~ ".repeat(64))}}});
    let answers = mcp_with_home(
        &fixture,
        format!("{request}\n").into_bytes(),
        Some(&"/".repeat(4096)),
    );
    assert_eq!(answers[0]["result"]["isError"], true);
}
