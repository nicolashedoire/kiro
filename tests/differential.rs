//! Differential tests: the engine against the system tools.
//!
//! For every command below, the engine must return byte for byte what `/bin/sh` and the
//! system programs return: same output, same exit code and, for failures, same messages.
//! The engine reproduces the BSD userland that ships with macOS, so these tests only run
//! there.
#![cfg(target_os = "macos")]

use itto::engine::{run_native, Session};
use std::path::{Path, PathBuf};
use std::process::Command;

const APP: &str = "\
import { helper } from './lib/util';

export const userId = 42;
export function load(id: number) {
  const a.b = helper(id);
  if (id > 0) {
    return a.b;
  }
  return null;
}

export class Store {
  private items: string[] = [];
  add(item: string) {
    this.items.push(item);
  }
}
";

const UTIL: &str = "\
export function helper(id: number) {
  return id * 2;
}

export const limit = 10;
";

/// A text without a final newline, with colons and both letter cases.
const NOTES: &str = "alpha: first\nbeta: second end\nAlpha: third\ngamma end";

fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("itto-diff-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/lib")).unwrap();
    std::fs::create_dir_all(dir.join("dir with space")).unwrap();
    std::fs::write(dir.join("src/app.ts"), APP).unwrap();
    std::fs::write(dir.join("src/lib/util.ts"), UTIL).unwrap();
    std::fs::write(dir.join("src/lib/data.json"), "{ \"export\": true }\n").unwrap();
    std::fs::write(dir.join("notes.txt"), NOTES).unwrap();
    std::fs::write(dir.join("empty.txt"), "").unwrap();
    std::fs::write(dir.join("blob.dat"), b"x\0y alpha\n").unwrap();
    std::fs::write(dir.join("dir with space/a b.txt"), "spaced\n").unwrap();
    dir
}

struct Outcome {
    stdout: Vec<u8>,
    stderr: String,
    code: i32,
}

fn system(dir: &Path, command: &str) -> Outcome {
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .output()
        .expect("cannot start /bin/sh");
    Outcome {
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code().unwrap_or(-1),
    }
}

fn engine(dir: &Path, command: &str) -> Outcome {
    let mut session = Session {
        dir: dir.to_path_buf(),
    };
    let reply = run_native(command, &mut session)
        .unwrap_or_else(|reason| panic!("the engine does not cover `{command}`: {reason}"));
    assert!(reply.native && reply.spawned == 0);
    Outcome {
        stdout: reply.stdout,
        stderr: reply.stderr,
        code: reply.code,
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Commands that succeed or simply find nothing: output and exit code must match.
const COMMANDS: &[&str] = &[
    // reading
    "cat src/app.ts",
    "cat -n notes.txt",
    "cat src/lib/util.ts notes.txt",
    "cat 'dir with space/a b.txt'",
    "sed -n '2,4p' src/app.ts",
    "sed -n '3p' notes.txt",
    "sed -n '2,$p' notes.txt",
    "sed -n '1p;3p' src/app.ts",
    "sed -n '4p' notes.txt",
    "head -3 src/app.ts",
    "head -n 2 src/app.ts notes.txt",
    "head src/app.ts",
    "tail -2 src/app.ts",
    "tail -n +3 notes.txt",
    "tail -n 1 notes.txt",
    // counting and cutting
    "wc -l src/app.ts",
    "wc src/app.ts notes.txt",
    "wc -c empty.txt",
    "wc -w notes.txt",
    "cut -c1-10 src/app.ts",
    "cut -d: -f1 notes.txt",
    "cut -d: -f2- notes.txt",
    "cut -c-5 notes.txt",
    // listing and printing
    "ls",
    "ls src",
    "ls -a src",
    "ls -A src/lib",
    "ls src/app.ts",
    "echo hello world",
    // searching
    "grep -n export src/app.ts",
    "grep export src/app.ts",
    "grep -c export src/app.ts src/lib/util.ts",
    "grep -rn export src",
    "grep -rn export src/",
    "grep -rl export .",
    "grep -r export",
    "grep -rn export . --include='*.ts'",
    "grep -rn export src --exclude-dir=lib",
    "grep -rn export . --exclude='*.json'",
    "grep -n -i EXPORT src/app.ts",
    "grep -n -v export src/app.ts",
    "grep -n -w id src/app.ts",
    "grep -n -x '}' src/app.ts",
    "grep -n -C1 return src/app.ts",
    "grep -n -A1 -B2 return src/app.ts",
    "grep -n -A 1 import src/app.ts",
    "grep -on '[a-z]*Id' src/app.ts",
    "grep -n -E '(const|let) [a-zA-Z]+' src/app.ts",
    "grep -n 'const\\|return' src/app.ts",
    "grep -n -F 'a.b' src/app.ts",
    "grep -n 'a.b' src/app.ts",
    "grep -n -m 2 e src/app.ts",
    "grep -n 'end$' notes.txt",
    "grep -n '^alpha' notes.txt",
    "grep -n '^[Aa]lpha' notes.txt",
    "grep -n -e first -e third notes.txt",
    "grep -H -n alpha notes.txt",
    "grep -h export src/app.ts src/lib/util.ts",
    "grep -L export src/app.ts notes.txt",
    "grep -rn alpha .",
    "grep -rnI alpha .",
    "grep -q export src/app.ts",
    "grep -n nothing-here src/app.ts",
    "grep -c nothing-here src/app.ts",
    // pipelines and lists
    "cat src/app.ts | grep -n const | head -2",
    "grep -rn export src | wc -l",
    "grep -n export src/app.ts | cut -d: -f1",
    "head -5 src/app.ts | tail -2",
    "cd src && grep -c export app.ts; cd lib && ls",
    "grep -n zzz src/app.ts || echo none",
    "grep -q export src/app.ts && echo found",
    "cat missing.txt 2>/dev/null; echo after",
    "echo first; echo second",
];

/// Commands that fail: the message and the exit code must match too.
const FAILURES: &[&str] = &[
    "cat missing.txt",
    "cat src/app.ts missing.txt",
    "sed -n 1p missing.txt",
    "head -2 missing.txt",
    "tail -2 missing.txt",
    "wc -l missing.txt",
    "cut -c1-3 missing.txt",
    "ls missing",
    "grep x missing.txt",
    "grep x src",
    "grep -s x missing.txt",
    "cat missing.txt 2>&1",
];

/// Compares the engine with the system on every command and reports all the differences at
/// once, so that one run on a new machine shows everything that behaves differently there.
fn compare(dir: &Path, commands: &[&str], with_messages: bool) {
    let mut differences = Vec::new();
    for command in commands {
        let expected = system(dir, command);
        let actual = engine(dir, command);
        if actual.stdout != expected.stdout {
            differences.push(format!(
                "output of `{command}`\n  system: {:?}\n  engine: {:?}",
                text(&expected.stdout),
                text(&actual.stdout)
            ));
        }
        if with_messages && actual.stderr != expected.stderr {
            differences.push(format!(
                "message of `{command}`\n  system: {:?}\n  engine: {:?}",
                expected.stderr, actual.stderr
            ));
        }
        if actual.code != expected.code {
            differences.push(format!(
                "exit code of `{command}`: system {}, engine {}",
                expected.code, actual.code
            ));
        }
    }
    assert!(
        differences.is_empty(),
        "{} difference(s) with the system tools:\n{}",
        differences.len(),
        differences.join("\n")
    );
}

#[test]
fn output_and_exit_code_match_the_system_tools() {
    let dir = fixture("output");
    compare(&dir, COMMANDS, false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn failures_match_the_system_tools() {
    let dir = fixture("failures");
    compare(&dir, FAILURES, true);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nothing_is_written_by_the_engine() {
    let dir = fixture("readonly");
    let mut session = Session { dir: dir.clone() };
    for command in [
        "rm notes.txt",
        "cat src/app.ts > copy.ts",
        "sed -i '' 's/a/b/' notes.txt",
        "echo x >> notes.txt",
        "mv notes.txt moved.txt",
        "tee out.txt",
    ] {
        assert!(
            run_native(command, &mut session).is_err(),
            "`{command}` must be refused"
        );
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        NOTES
    );
    assert!(!dir.join("copy.ts").exists());
    assert!(!dir.join("moved.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
