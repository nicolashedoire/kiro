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

#[test]
fn directional_boundaries_and_alphabetic_extensions_are_refused() {
    for pattern in [
        r"\<alpha",
        r"\>alpha",
        "[[:<:]]alpha",
        "alpha[[:>:]]",
        r"\A",
        r"\z",
        r"\d",
        r"\w",
        r"\b",
        r"\n",
    ] {
        for syntax in [Syntax::Basic, Syntax::Extended] {
            let o = grep::parse(&[pattern.to_string()], syntax).unwrap();
            assert!(
                grep::run(&o, Path::new("."), Some(b"alpha\n")).is_err(),
                "{syntax:?} {pattern}"
            );
        }
    }
    // Escapes remain literal with fixed-string matching.
    let literal = grep::run(&options(&["-F", r"\A"]), Path::new("."), Some(b"\\A\n")).unwrap();
    assert_eq!(literal.stdout, b"\\A\n");
}

#[test]
fn locale_matrices_compare_supported_outputs_and_report_refusals_separately() {
    for locale in ["C", "en_US.UTF-8"] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "locale_matrix_worker", "--nocapture"])
            .env("KIRO_GREP_MATRIX_LOCALE", locale)
            .env("LC_ALL", locale)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "locale {locale}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("{}", String::from_utf8_lossy(&output.stdout));
    }
}

/// Each locale runs in a separate process; tests never change global environment state.
#[test]
fn locale_matrix_worker() {
    let Ok(locale) = std::env::var("KIRO_GREP_MATRIX_LOCALE") else {
        return;
    };
    if locale == "en_US.UTF-8" {
        for (args, input) in [
            (vec!["[[:alpha:]]"], "é\n"),
            (vec!["[a-z]"], "é\n"),
            (vec!["-w", "alpha"], "漢alpha\n"),
            (vec!["-i", "alpha"], "é alpha\n"),
        ] {
            assert!(grep::run(&options(&args), Path::new("."), Some(input.as_bytes())).is_err());
        }
        let literal = grep::run(
            &options(&["-F", "é"]),
            Path::new("."),
            Some("é\n漢\n".as_bytes()),
        )
        .unwrap();
        assert_eq!(literal.stdout, "é\n".as_bytes());
    }
    let patterns = [
        (Syntax::Basic, "alpha"),
        (Syntax::Basic, "^alpha$"),
        (Syntax::Basic, "a.*a"),
        (Syntax::Basic, "[[:alpha:]]"),
        (Syntax::Basic, "[[:digit:]]"),
        (Syntax::Basic, "[[:space:]]"),
        (Syntax::Basic, "[[:alnum:]]"),
        (Syntax::Basic, "[^a]"),
        (Syntax::Basic, "[a-z]"),
        (Syntax::Basic, r"a\|ab"),
        (Syntax::Basic, r"\>alpha"),
        (Syntax::Basic, r"\A"),
        (Syntax::Extended, "a|ab"),
        (Syntax::Extended, "a+"),
        (Syntax::Extended, "."),
        (Syntax::Extended, "(alpha|beta)"),
        (Syntax::Extended, "[[:alpha:]]"),
        (Syntax::Fixed, "alpha"),
        (Syntax::Fixed, "a|ab"),
        (Syntax::Fixed, "é"),
    ];
    let modifiers: &[&[&str]] = &[
        &[],
        &["-n"],
        &["-i"],
        &["-v"],
        &["-c"],
        &["-l"],
        &["-L"],
        &["-q"],
        &["-o"],
        &["-w"],
        &["-x"],
        &["-n", "-i"],
        &["-n", "-v"],
        &["-n", "-H"],
        &["-n", "-h"],
        &["-c", "-H"],
        &["-m1"],
        &["-m2"],
        &["-A1"],
        &["-B1"],
        &["-C1"],
        &["-c", "-v"],
        &["-l", "-v"],
        &["-L", "-v"],
        &["-q", "-v"],
        &["-m0"],
    ];
    let corpora: &[&[u8]] = &[
        b"alpha\nalphabet\n_alpha\nalpha_\n(a)\nabc123Id\n123Id\n\n",
        b"alpha\nbeta\nAlpha\ngamma",
        "é\nÉ\n漢alpha\nalpha漢\nalpha\ne\u{301}\n".as_bytes(),
        b"\0alpha\nz\n",
    ];
    let mut compared = 0;
    let mut refused = 0;
    let mut differences = Vec::new();
    for (syntax, pattern) in patterns {
        for flags in modifiers {
            let mut args: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
            args.push(
                match syntax {
                    Syntax::Basic => "-G",
                    Syntax::Extended => "-E",
                    Syntax::Fixed => "-F",
                }
                .to_string(),
            );
            args.push("-e".to_string());
            args.push(pattern.to_string());
            for (corpus, input) in corpora.iter().enumerate() {
                let o = match grep::parse(&args, syntax) {
                    Ok(o) => o,
                    Err(_) => {
                        refused += 1;
                        continue;
                    }
                };
                let actual = match grep::run(&o, Path::new("."), Some(input)) {
                    Ok(r) => r,
                    Err(_) => {
                        refused += 1;
                        continue;
                    }
                };
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                let expected = system(&refs, input);
                compared += 1;
                if actual.stdout != expected.stdout
                    || actual.stderr.as_bytes() != expected.stderr
                    || Some(actual.code) != expected.status.code()
                {
                    differences.push(format!("{locale} corpus={corpus} {args:?}: actual={:?}/{:?}/{} expected={:?}/{:?}/{:?}", actual.stdout,actual.stderr,actual.code,expected.stdout,expected.stderr,expected.status.code()));
                }
            }
        }
    }
    println!("{locale}: {compared} native cases compared; {refused} refusals counted separately; {} differences", differences.len());
    let minimum = if locale == "C" { 1700 } else { 1300 };
    assert!(
        compared >= minimum,
        "matrix must retain verified native coverage"
    );
    assert!(refused > 0);
    compare_filename_patterns(&locale);
    assert!(
        differences.is_empty(),
        "{}",
        differences
            .iter()
            .take(15)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn pathological_filename_pattern_does_not_block_the_mcp_server() {
    use serde_json::{json, Value};
    use std::io::Read;
    use std::time::{Duration, Instant};
    let dir = std::env::temp_dir().join(format!("kiro-grep-availability-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a".repeat(30)), "needle\n").unwrap();
    let pattern = format!("{}b", "*a".repeat(30));
    let messages = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run","arguments":{"command":format!("grep -rn needle . --include='{pattern}'"),"exact":true}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"ping"}),
    ];
    let input = messages
        .iter()
        .map(|m| format!("{m}\n"))
        .collect::<String>();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kiro"))
        .args(["mcp", "--project"])
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(100_000).read_to_end(&mut bytes).unwrap();
        bytes
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(3) {
            child.kill().unwrap();
            child.wait().unwrap();
            let _ = reader.join();
            let _ = std::fs::remove_dir_all(&dir);
            panic!("filename glob blocked the MCP request and following ping");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let bytes = reader.join().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(status.success());
    let answers: Vec<Value> = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(answers.len(), 3);
    assert_eq!(answers[1]["result"]["isError"], false);
    assert_eq!(answers[1]["result"]["content"][0]["text"], "[exit code 1]");
    assert_eq!(answers[2]["id"], 3);
    assert_eq!(answers[2]["result"], json!({}));
}

fn compare_filename_patterns(locale: &str) {
    let root =
        std::env::temp_dir().join(format!("kiro-glob-matrix-{locale}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let patterns = [
        "*.rs",
        "a?.rs",
        "[ab]*",
        "[!a]*",
        "[^a]*",
        "[]a]*",
        "[-a]*",
        "[a-]*",
        "?.rs",
        "??.rs",
        r"\[literal\].rs",
        "[a-z]*",
        "a*b",
    ];
    let mut compared = 0;
    let mut refused = 0;
    let mut differences = Vec::new();
    for (fixture, names) in [
        (
            "ascii",
            vec![
                "alpha.rs",
                "beta.rs",
                "a1.rs",
                "[literal].rs",
                "a-.rs",
                ".hidden.rs",
                "aaab",
            ],
        ),
        ("unicode", vec!["alpha.rs", "é.rs", "漢.rs", "éé.rs"]),
    ] {
        let dir = root.join(fixture);
        std::fs::create_dir_all(dir.join("keep")).unwrap();
        std::fs::create_dir_all(dir.join("skip")).unwrap();
        for name in names {
            std::fs::write(dir.join(name), "needle\n").unwrap();
        }
        for name in ["keep", "skip"] {
            std::fs::write(dir.join(name).join("nested.rs"), "needle\n").unwrap();
        }
        for pattern in patterns {
            for flag in ["--include", "--exclude"] {
                let args = vec![
                    "-rn".to_string(),
                    "-e".to_string(),
                    "needle".to_string(),
                    format!("{flag}={pattern}"),
                    "--exclude-dir=skip".to_string(),
                    ".".to_string(),
                ];
                let options = match grep::parse(&args, Syntax::Basic) {
                    Ok(options) => options,
                    Err(_) => {
                        refused += 1;
                        continue;
                    }
                };
                let actual = match grep::run(&options, &dir, None) {
                    Ok(r) => r,
                    Err(_) => {
                        refused += 1;
                        continue;
                    }
                };
                let expected = Command::new("/usr/bin/grep")
                    .args(&args)
                    .current_dir(&dir)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                compared += 1;
                if actual.stdout != expected.stdout
                    || actual.stderr.as_bytes() != expected.stderr
                    || Some(actual.code) != expected.status.code()
                {
                    differences.push(format!(
                        "{locale} {fixture} {args:?}: actual={:?}/{:?}/{} expected={:?}/{:?}/{:?}",
                        String::from_utf8_lossy(&actual.stdout),
                        actual.stderr,
                        actual.code,
                        String::from_utf8_lossy(&expected.stdout),
                        String::from_utf8_lossy(&expected.stderr),
                        expected.status.code()
                    ));
                }
            }
        }
        // Filename filters also apply to explicit operands, including a missing operand.
        for flag in [
            "--include=*.rs",
            "--exclude=*.rs",
            "--include=absent",
            "--exclude=absent",
        ] {
            let args = [
                "-n",
                "-e",
                "needle",
                flag,
                "alpha.rs",
                "absent",
                "keep/nested.rs",
            ];
            let options = options(&args);
            let actual = grep::run(&options, &dir, None).unwrap();
            let expected = Command::new("/usr/bin/grep")
                .args(args)
                .current_dir(&dir)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            compared += 1;
            if actual.stdout != expected.stdout
                || actual.stderr.as_bytes() != expected.stderr
                || Some(actual.code) != expected.status.code()
            {
                differences.push(format!(
                    "{locale} explicit {flag}: actual={:?}/{:?}/{} expected={:?}/{:?}/{:?}",
                    actual.stdout,
                    actual.stderr,
                    actual.code,
                    expected.stdout,
                    expected.stderr,
                    expected.status.code()
                ));
            }
        }
    }
    std::fs::remove_dir_all(&root).unwrap();
    println!("{locale}: {compared} filename-filter cases compared; {refused} refusals counted separately; {} differences", differences.len());
    assert!(compared >= if locale == "C" { 50 } else { 40 });
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}
