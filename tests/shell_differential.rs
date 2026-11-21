//! Cross-product checks of shell lists, comments, quoting and logical directory traversal.
#![cfg(target_os = "macos")]

use kiro::engine::{run, run_native, Session};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("kiro-shell-matrix-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("physical/inside")).unwrap();
        std::fs::write(dir.join("notes.txt"), "alpha\nbeta\n").unwrap();
        std::fs::write(dir.join("hash#file.txt"), "hash\n").unwrap();
        std::fs::write(dir.join("file #name.txt"), "space and hash\n").unwrap();
        std::fs::write(dir.join("marker"), "LOGICAL\n").unwrap();
        std::fs::write(dir.join("physical/marker"), "PHYSICAL\n").unwrap();
        std::fs::write(dir.join("physical/inside/marker"), "INSIDE\n").unwrap();
        std::os::unix::fs::symlink("physical/inside", dir.join("link")).unwrap();
        Self(dir)
    }

    fn session(&self) -> Session {
        Session {
            dir: self.0.clone(),
        }
    }

    fn reference(&self, shell: &str, command: &str) -> std::process::Output {
        let mut process = Command::new(shell);
        if shell.ends_with("zsh") {
            process.arg("-f");
        }
        process
            .args(["-c", command])
            .current_dir(&self.0)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .env("HOME", std::env::var_os("HOME").unwrap())
            .output()
            .unwrap()
    }

    fn compare(&self, command: &str) {
        let actual = run_native(command, &mut self.session())
            .unwrap_or_else(|e| panic!("supported matrix case refused: {command:?}: {e}"));
        for shell in ["/bin/sh", "/bin/zsh"] {
            let expected = self.reference(shell, command);
            assert_eq!(actual.stdout, expected.stdout, "{shell} stdout {command:?}");
            assert_eq!(
                actual.stderr.as_bytes(),
                expected.stderr,
                "{shell} stderr {command:?}"
            );
            assert_eq!(
                Some(actual.code),
                expected.status.code(),
                "{shell} status {command:?}"
            );
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn comments_and_quoted_hashes_match_both_shells() {
    let fixture = Fixture::new("comments");
    let commands = [
        "cat notes.txt # explanation",
        "cat notes.txt # ignored; cat absent\necho after",
        "cat hash#file.txt # comment",
        "cat 'file #name.txt' # comment",
        "cat \"file #name.txt\" # comment",
        r"cat file\ \#name.txt # comment",
        "echo 'a # b' # comment",
        "echo \"a # b\" # comment",
        "echo value#hash # comment",
        "echo ''#hash # comment",
        "# leading comment\ncat notes.txt",
        "echo first && # conditional continuation\necho second",
        "grep -q absent notes.txt || # conditional continuation\necho missing",
        "echo first # ignored backslash\\\necho second",
    ];
    for command in commands {
        fixture.compare(command);
    }
    eprintln!(
        "comments: {} native cases, {} shell comparisons",
        commands.len(),
        commands.len() * 2
    );
}

#[test]
fn valid_lists_and_pipelines_have_a_generated_differential_matrix() {
    let fixture = Fixture::new("lists");
    let first = [
        "echo first",
        "grep -q alpha notes.txt",
        "grep -q absent notes.txt",
    ];
    let second = [
        "echo second",
        "cat notes.txt",
        "cat notes.txt | head -1",
        "echo 'quoted ; && || # text'",
    ];
    let links = ["; ", " && ", " || ", "\n", " &&\n", " ||\n"];
    let mut checked = 0;
    for a in first {
        for b in second {
            for link in links {
                for suffix in ["", ";", "\n", "\n\n"] {
                    fixture.compare(&format!("{a}{link}{b}{suffix}"));
                    checked += 1;
                }
            }
        }
    }
    eprintln!(
        "lists/pipelines: {checked} native cases, {} shell comparisons",
        checked * 2
    );
}

#[test]
fn malformed_lists_are_refused_before_any_directory_change() {
    let fixture = Fixture::new("invalid");
    let commands = [
        "&& echo after",
        "|| echo after",
        "; echo after",
        "echo first &&",
        "echo first ||",
        "echo first &&\n",
        "echo first ||\n",
        "echo first ;; echo after",
        "echo first; ; echo after",
        "echo first && || echo after",
        "echo first && ; echo after",
        "echo first\n&& echo after",
        "echo first |",
        "| echo after",
        "echo first | | echo after",
        "echo first &&&& echo after",
        "cd physical &&",
        "cd physical; ; echo after",
        "cd physical && # no following command",
        "echo first || | echo after",
        "echo first || echo after |",
        "cd physical || | echo after",
    ];
    for command in commands {
        let mut session = fixture.session();
        assert!(run_native(command, &mut session).is_err(), "{command:?}");
        assert_eq!(session.dir, fixture.0, "{command:?}");
        // zsh permits some incomplete lists that /bin/sh rejects. The native subset
        // deliberately refuses these instead of choosing one shell's interpretation.
        assert!(
            !fixture.reference("/bin/sh", command).status.success(),
            "{command:?}"
        );
    }
    eprintln!(
        "malformed or nonportable grammar: {} explicit refusals checked against /bin/sh",
        commands.len()
    );
}

#[test]
fn logical_cd_through_symlinks_matches_both_shells() {
    let fixture = Fixture::new("logical");
    let commands = [
        "cd link; cat marker",
        "cd link; cd ..; cat marker",
        "cd link/..; cat marker",
        "cd ./link/../link; cat marker",
        "cd physical; cd inside; cd ..; cat marker",
        "cd link; cd ../physical; cat marker",
        "cd link; cd ../..; cd physical; cat marker",
    ];
    // The penultimate directory above the fixture need not contain our data; build that
    // case with an absolute path rather than relying on its temporary parent.
    for command in &commands[..commands.len() - 1] {
        fixture.compare(command);
    }
    fixture.compare(&format!(
        "cd link; cd ../..; cd '{}'; cat marker",
        fixture.0.display()
    ));
    for command in ["cd -P link", "cd -- link", "cd ''", "cd missing/.."] {
        assert!(
            run_native(command, &mut fixture.session()).is_err(),
            "{command}"
        );
    }
}

#[test]
fn logical_directory_survives_cli_startup_and_fallback() {
    let fixture = Fixture::new("pwd");
    let link = fixture.0.join("link");
    let output = Command::new(env!("CARGO_BIN_EXE_kiro"))
        .args(["run", "--stats", "cd ..; cat marker"])
        .current_dir(&link)
        .env("PWD", &link)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"LOGICAL\n");
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("inside the engine"));
    let mut session = fixture.session();
    run_native("cd link", &mut session).unwrap();
    let reply = run("cd ..; /bin/cat marker", &mut session);
    assert!(!reply.native);
    assert_eq!(reply.stdout, b"LOGICAL\n");
}

#[test]
fn mcp_project_symlink_preserves_logical_parent() {
    let fixture = Fixture::new("mcp-pwd");
    let mut child = Command::new(env!("CARGO_BIN_EXE_kiro"))
        .args(["mcp", "--project"])
        .arg(fixture.0.join("link"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"run","arguments":{"command":"cd ..; cat marker","exact":true}}})).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(answer["result"]["isError"], false);
    assert_eq!(answer["result"]["content"][0]["text"], "LOGICAL\n");
}
