//! BSD grep comparisons for the supported forms, and explicit refusal of ambiguous forms.
#![cfg(target_os = "macos")]

use kiro::grep::{self, Syntax};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn options(arguments: &[&str]) -> grep::Options {
    let args: Vec<String> = arguments.iter().map(|s| s.to_string()).collect();
    grep::parse(&args, Syntax::Basic).unwrap()
}

fn system(arguments: &[&str], input: &[u8]) -> std::process::Output {
    let mut child = Command::new("/usr/bin/grep")
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn supported_word_and_greedy_matches_equal_bsd_grep() {
    let input = b"alpha\nalphabet\n_alpha\nalpha_\n(alpha)\nabc123Id\n123Id\n\n";
    for args in [
        vec!["-wn", "alpha"],
        vec!["-Eon", "[a-z]*Id"],
        vec!["-Eon", "a*"],
        vec!["-Eon", "[a-z]+"],
        vec!["-Fon", "alpha"],
        vec!["-on", "a|ab"],
        vec!["--color=never", "alpha"],
    ] {
        let expected = system(&args, input);
        let actual = grep::run(&options(&args), Path::new("."), Some(input)).unwrap();
        assert_eq!(actual.stdout, expected.stdout, "{args:?}");
        assert_eq!(actual.stderr.as_bytes(), expected.stderr, "{args:?}");
        assert_eq!(Some(actual.code), expected.status.code(), "{args:?}");
    }
}

#[test]
fn audited_mismatches_are_refused_instead_of_returning_wrong_results() {
    let expected = system(&["-Eo", "a|ab"], b"ab\n");
    assert_eq!(expected.stdout, b"ab\n");
    assert!(grep::run(&options(&["-Eo", "a|ab"]), Path::new("."), Some(b"ab\n")).is_err());

    let input = b"@\na@b\n @ \n";
    let expected = system(&["-w", "@"], input);
    assert_eq!(expected.stdout, b"@\n @ \n");
    assert!(grep::run(&options(&["-w", "@"]), Path::new("."), Some(input)).is_err());
}
