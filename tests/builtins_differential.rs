//! Cross-product BSD comparisons for file boundaries, encodings and option families.
//! Positive cases must stay native: a fallback is not counted as compatibility evidence.
//! Locale-sensitive refusals have their own tests and are not hidden as passing cases.
#![cfg(target_os = "macos")]

use kiro::engine::{run_native, Session};
use std::path::PathBuf;
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "kiro-builtins-matrix-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for (file, bytes) in [
            ("empty", &b""[..]),
            ("partial", &b"alpha"[..]),
            ("terminated", &b"beta\n"[..]),
            ("lines", &b"one:1\ntwo:2\nthree:3\n"[..]),
            ("blanks", &b"\n\nlast"[..]),
            ("crlf", &b"first:1\r\nsecond:2\r\n"[..]),
            ("unicode", "é:a\n漢:b".as_bytes()),
            ("spaces", "a\u{a0}b\n".as_bytes()),
        ] {
            std::fs::write(root.join(file), bytes).unwrap();
        }
        Self(root)
    }

    fn session(&self) -> Session {
        Session {
            dir: self.0.clone(),
        }
    }

    fn compare(&self, command: &str, locale: &str) {
        let expected = Command::new("/bin/sh")
            .args(["-c", command])
            .current_dir(&self.0)
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", locale)
            .output()
            .unwrap();
        // A subprocess also isolates the native locale; changing this test process's
        // environment would race with other tests and leave its C locale unchanged.
        let actual = Command::new(env!("CARGO_BIN_EXE_kiro"))
            .args(["run", "--stats", command])
            .current_dir(&self.0)
            .env("LC_ALL", locale)
            .output()
            .unwrap();
        let stderr = String::from_utf8(actual.stderr).unwrap();
        let stats = stderr.rfind("[kiro] ").expect("missing native statistics");
        assert!(
            stderr[stats..].starts_with("[kiro] inside the engine, 0 program started,"),
            "positive case was refused, not compared: locale={locale}, {command}: {stderr}"
        );
        assert_eq!(
            actual.stdout, expected.stdout,
            "stdout: locale={locale}, {command}"
        );
        assert_eq!(
            &stderr.as_bytes()[..stats],
            expected.stderr,
            "stderr: locale={locale}, {command}"
        );
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "status: locale={locale}, {command}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const LOCALES: [&str; 2] = ["C", "en_US.UTF-8"];

#[test]
fn sed_addresses_preserve_global_lines_and_each_file_boundary() {
    let fixture = Fixture::new("sed");
    let scripts = [
        "1p", "2p", "3p", "$p", "1,2p", "2,$p", "3,1p", "1p;1p", "1p;3p", "$,$p", "$p;1p", "1,1p",
        "1,3p;2p",
    ];
    let sources = [
        "empty",
        "partial",
        "terminated",
        "lines",
        "blanks",
        "crlf",
        "unicode",
        "partial terminated",
        "partial partial",
        "partial empty",
        "empty partial",
        "partial empty terminated",
        "blanks partial empty",
        "lines partial terminated",
        "unicode partial",
        "partial missing terminated",
    ];
    let mut compared = 0;
    for locale in LOCALES {
        for script in scripts {
            for source in sources {
                fixture.compare(&format!("sed -n '{script}' {source}"), locale);
                compared += 1;
            }
            for source in ["partial", "partial terminated", "unicode", "blanks"] {
                fixture.compare(&format!("cat {source} | sed -n '{script}'"), locale);
                compared += 1;
            }
        }
    }
    eprintln!("sed matrix: {compared} native stdout/stderr/status comparisons");
}

#[test]
fn cut_modes_preserve_file_boundaries_and_pipe_streams() {
    let fixture = Fixture::new("cut");
    let forms = [
        "-c1",
        "-c1-2",
        "-c2-",
        "-c1,3",
        "-b1",
        "-b1-2",
        "-b2-",
        "-d: -f1",
        "-d: -f2-",
        "-d: -f1,3",
        "-s -d: -f1",
        "-f1",
        "-s -f1",
    ];
    let sources = [
        "empty",
        "partial",
        "terminated",
        "lines",
        "blanks",
        "crlf",
        "unicode",
        "partial terminated",
        "partial partial",
        "partial empty",
        "empty partial",
        "unicode terminated",
        "lines partial unicode",
        "partial missing terminated",
    ];
    let mut compared = 0;
    for locale in LOCALES {
        for form in forms {
            for source in sources {
                fixture.compare(&format!("cut {form} {source}"), locale);
                compared += 1;
            }
            for source in ["partial terminated", "unicode", "lines partial"] {
                fixture.compare(&format!("cat {source} | cut {form}"), locale);
                compared += 1;
            }
        }
    }
    eprintln!("cut matrix: {compared} native stdout/stderr/status comparisons");
}

#[test]
fn head_tail_counts_and_wc_ascii_words_match_bsd() {
    let fixture = Fixture::new("counts");
    let sources = [
        "empty",
        "partial",
        "terminated",
        "lines",
        "blanks",
        "crlf",
        "unicode",
    ];
    let mut compared = 0;
    for locale in LOCALES {
        for source in sources {
            for count in ["1", "2", "8", "+2", "0002"] {
                fixture.compare(&format!("head -n {count} {source}"), locale);
                fixture.compare(&format!("cat {source} | head -n {count}"), locale);
                compared += 2;
            }
            for count in ["0", "1", "2", "+0", "+1", "+2", "-0", "-1", "-2"] {
                fixture.compare(&format!("tail -n {count} {source}"), locale);
                fixture.compare(&format!("cat {source} | tail -n {count}"), locale);
                compared += 2;
            }
            for form in ["-l", "-c", "-lc"] {
                fixture.compare(&format!("wc {form} {source}"), locale);
                compared += 1;
            }
            if source != "unicode" {
                for form in ["", "-w", "-lw", "-wc", "-lwc"] {
                    fixture.compare(&format!("wc {form} {source}"), locale);
                    fixture.compare(&format!("cat {source} | wc {form}"), locale);
                    compared += 2;
                }
            }
        }
        for source in [
            "partial terminated",
            "empty partial",
            "partial empty",
            "lines blanks crlf",
            "partial missing terminated",
        ] {
            fixture.compare(&format!("head -2 {source}"), locale);
            fixture.compare(&format!("wc {source}"), locale);
            fixture.compare(&format!("wc -l {source}"), locale);
            compared += 3;
        }
    }
    eprintln!("counting matrix: {compared} native stdout/stderr/status comparisons");
}

#[test]
fn locale_sensitive_and_ambiguous_forms_are_explicitly_refused() {
    let fixture = Fixture::new("refusals");
    let commands = [
        "wc -w spaces",
        "wc unicode",
        "cat unicode | wc -w",
        "head -n -2 lines",
        "head -n 0 lines",
        "head -0 lines",
        "head -n +0 lines",
        "head -n --2 lines",
        "head -n '2 ' lines",
        "head -n 18446744073709551615 lines",
        "tail -n --2 lines",
        "tail -n ++2 lines",
        "tail -n 18446744073709551615 lines",
        "cut -c1 -f1 lines",
        "cut -c1 -b2 lines",
        "cut -d: -f1 -f2 lines",
        "cut -c1 -d: lines",
        "cut -s -c1 lines",
        "cut -c+1 lines",
        "sed -n '+1p' lines",
        "sed -n '1,+2p' lines",
    ];
    for command in commands {
        assert!(
            run_native(command, &mut fixture.session()).is_err(),
            "must refuse {command}"
        );
    }
    // Verify the audited locale-sensitive case is genuinely distinct from ASCII splitting.
    let expected = Command::new("/usr/bin/wc")
        .args(["-w", "spaces"])
        .current_dir(&fixture.0)
        .env("LC_ALL", "en_US.UTF-8")
        .output()
        .unwrap();
    assert!(expected.status.success());
    assert_eq!(expected.stdout, b"       2 spaces\n");
    eprintln!(
        "explicit refusals: {} (excluded from native comparison counts)",
        commands.len()
    );
}
