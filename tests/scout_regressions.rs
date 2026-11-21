use kiro::scout::{scout, Request};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "kiro-scout-regression-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ask(dir: &Path, budget: usize) -> String {
    scout(
        dir,
        &Request {
            terms: vec!["needle".to_string()],
            paths: Vec::new(),
            budget,
        },
    )
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
}

#[test]
fn git_discovery_does_not_run_a_configured_fsmonitor() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("fsmonitor");
    let dir = &fixture.0;
    git(dir, &["init", "--quiet"]);
    std::fs::write(dir.join("tracked.rs"), "pub const needle: u8 = 1;\n").unwrap();
    git(dir, &["add", "tracked.rs"]);
    let hook = dir.join(".git/monitor.sh");
    let marker = dir.join(".git/monitor.sh.marker");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf ran > \"$0.marker\"\nprintf 'token\\0'\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(dir, &["config", "core.fsmonitor", hook.to_str().unwrap()]);

    // Establish that the fixture would execute its hook without scout's overrides.
    git(
        dir,
        &["ls-files", "--cached", "--others", "--exclude-standard"],
    );
    assert!(marker.exists(), "control Git command did not run the hook");
    std::fs::remove_file(&marker).unwrap();

    let answer = ask(dir, 4000);
    assert!(answer.contains("tracked.rs"), "{answer}");
    assert!(!marker.exists(), "scout ran the fsmonitor hook");
}

#[test]
fn an_empty_git_listing_does_not_search_ignored_files() {
    let fixture = Fixture::new("ignored");
    git(&fixture.0, &["init", "--quiet"]);
    std::fs::write(fixture.0.join(".gitignore"), "*\n").unwrap();
    std::fs::write(fixture.0.join("ignored.txt"), "needle\n").unwrap();
    let answer = ask(&fixture.0, 4000);
    assert!(answer.starts_with("scout: nothing found"), "{answer}");
    assert!(!answer.contains("ignored.txt"), "{answer}");
}

#[test]
fn the_answer_budget_is_strict_for_unicode_and_extreme_values() {
    let fixture = Fixture::new("budget");
    std::fs::write(
        fixture.0.join("unicode.rs"),
        format!("pub fn needle() {{\n  // {}\n}}\n", "é💡".repeat(100)),
    )
    .unwrap();
    for budget in [0, 1, 10, 42, 128, 512, usize::MAX] {
        let answer = ask(&fixture.0, budget);
        assert!(
            answer.len() <= budget.min(64 * 1024),
            "{budget}: {}",
            answer.len()
        );
        if (42..512).contains(&budget) {
            assert!(answer.contains("truncated"), "{answer}");
        }
    }
}

#[test]
fn large_scans_are_reported_as_incomplete() {
    let fixture = Fixture::new("scan-limit");
    let mut contents = vec![b' '; 400_000];
    contents[..7].copy_from_slice(b"needle\n");
    for index in 0..50 {
        std::fs::write(fixture.0.join(format!("file-{index:02}.rs")), &contents).unwrap();
    }
    let answer = ask(&fixture.0, 4000);
    assert!(answer.starts_with("[search incomplete:"), "{answer}");
    assert!(answer.len() <= 4000);
}

#[test]
fn oversized_queries_are_refused_instead_of_silently_dropping_terms() {
    let fixture = Fixture::new("query-limit");
    for terms in [vec!["needle".to_string(); 61], vec!["x".repeat(513)]] {
        let answer = scout(
            &fixture.0,
            &Request {
                terms,
                paths: Vec::new(),
                budget: 4000,
            },
        );
        assert!(
            answer.starts_with("scout: too many or oversized terms"),
            "{answer}"
        );
    }
}
