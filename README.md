![kiro - A terminal engine for AI coding agents](assets/kiro-banner.jpg)

# kiro

**A terminal engine for AI coding agents: same commands, no shell, fewer round trips.**

[![CI](https://github.com/nicolashedoire/kiro/actions/workflows/ci.yml/badge.svg)](https://github.com/nicolashedoire/kiro/actions/workflows/ci.yml)

> Status: experimental. macOS only for now. Compatibility is checked against the system
> tools, but a passing corpus is not a guarantee for every shell expression. Read the
> [limits](#limits) before relying on it.
>
> Not affiliated with [Kiro](https://kiro.dev), the IDE from AWS.

## Why

When an AI agent reads one file through a terminal, the command travels through a chain: the
harness reviews it, hooks run, a fresh shell starts and loads its profile, wrappers start, and
only then does the real program run. On top of that, **every tool call is a full round trip
with the model that re-reads the whole conversation**. An agent that explores a code base one
`grep` at a time pays that price a dozen times before it changes a single line.

The idea comes from container engines such as [Kiri](https://github.com/libracoder/kiri): keep
the interface people already use, and replace the chain behind it. kiro keeps the commands the
agent already writes (`grep`, `sed -n`, `cat`, …) and removes what sits behind them.

## What it does

- **`run`: a read-only terminal with no shell.** `cat`, `sed -n`, `head`, `tail`, `wc`, `cut`,
  `ls`, `grep`, pipelines and `;` / `&&` / `||` lists run inside one long-lived process. No
  shell or program is started for supported forms. Differential tests check the output
  against the system tools.
  Several commands fit in one call, so one round trip brings back several answers.
- **`scout`: the map of an area of the code in one call.** Give it the words you care about;
  it returns the files that mention them, ranked, with the matching lines and the function
  each one sits in. It replaces the dozen searches an agent usually starts with.
- **An MCP server** that gives both to an agent such as Claude Code.

Three rules hold it together:

1. **Same commands.** The agent has nothing to learn.
2. **Same results, or no result.** A command is either run entirely inside the engine or not
   at all. Anything not covered is refused (MCP) or handed to your regular shell (CLI), never
   approximated. A [differential test suite](tests/differential.rs) compares the engine with
   the system tools, byte for byte.
3. **Designed for reading.** Native `run` executes fixed built-ins without starting a
   program. `scout` invokes system Git to discover files, with hooks and filesystem-monitor
   commands disabled, then reads the files. These tools do not provide an OS sandbox or
   restrict access to the project directory; normal filesystem permissions still apply.

## Results

**Real AI sessions, measured on 21 November 2026.** Thirty fresh Claude Sonnet 4.6 sessions
on three source-investigation tasks across Kiro and Fastify, five repetitions per setup
and task. The same model/prompt used either the system terminal or the terminal plus Kiro MCP.

![Measured end-to-end AI benchmark](assets/benchmarks/ai-benchmark-overview.png)

Median elapsed time was **53.0 s with the terminal, 36.4 s with Kiro MCP** (31.4% lower in
this sample). All required paths were returned in **15/15 terminal sessions and 14/15 Kiro
sessions**. Path coverage is not a semantic-correctness score; manual review found explanation
errors in both conditions. All sessions remain included. These results do not establish a
universal speedup or remove the tool's experimental limitations.

<details>
<summary>See every task, session and reported model usage</summary>

![Per-task AI session times](assets/benchmarks/ai-benchmark-tasks.png)

![Reported model turns, tokens and cost](assets/benchmarks/ai-benchmark-effort.png)

</details>

[Method, pinned datasets, prompts, per-session data and reproduction scripts](docs/benchmarks/2026-11-21-sonnet46/README.md).

The repository includes differential tests and a public, temporary benchmark fixture:

```sh
cargo run --release --locked --example replay -- --fixture
```

This checks native stdout, stderr and exit status against `/bin/sh` and the macOS tools,
without private sessions or model calls. The optional MCP mode compares formatted UTF-8
responses, not raw binary streams. It is a small reproducible check, not a representative
performance study.

Earlier measurements on one private TypeScript monorepo suggested fewer model round trips
and lower latency. They predate the audit fixes, have not been revalidated, and do not
establish the performance or accuracy of the current version. The historical results,
method, limitations and reproduction commands are in [docs/benchmarks.md](docs/benchmarks.md).

## Install

Requires a Rust toolchain ([rustup.rs](https://rustup.rs)).

```sh
cargo install --git https://github.com/nicolashedoire/kiro
```

Or from a clone:

```sh
git clone https://github.com/nicolashedoire/kiro
cd kiro
cargo build --release      # the binary is target/release/kiro
```

## Use

### From the command line

```sh
$ kiro run --stats "grep -rn 'export function' src | head -3"
src/api.ts:12:export function load(id: number) {
src/api.ts:40:export function save(item: Item) {
src/store.ts:8:export function createStore() {
[kiro] inside the engine, 0 program started, 0.41 ms

$ kiro run --stats "ls -la"
…
[kiro] handed to the regular shell (ls -l), 2 programs started, 9.80 ms

$ kiro scout "retry" "max attempts" "backoff"
SCOUT - 7 files mention these terms (412 text files read; 3 skipped; 0 read errors)
…
```

`--stats` tells whether the command ran inside the engine and how long it took.

### With Claude Code

In the project where you want it:

```sh
claude mcp add kiro -- kiro mcp
```

To let the agent call it without a prompt each time, allow the two read-only tools in your
Claude Code settings:

```json
{ "permissions": { "allow": ["mcp__kiro__run", "mcp__kiro__scout"] } }
```

Remove it with `claude mcp remove kiro`. Any other MCP client works the same way: the server
speaks JSON-RPC over standard input and output (`kiro mcp --project DIR`).

### What the agent sees

```jsonc
// one call…
{ "name": "run", "arguments": { "commands": [
    "grep -n 'export function' src/api.ts",
    "sed -n '12,30p' src/api.ts",
    "wc -l src/*.ts"            // not covered (glob): refused, nothing is executed
] } }
```

```text
$ grep -n 'export function' src/api.ts
12:export function load(id: number) {
40:export function save(item: Item) {
$ sed -n '12,30p' src/api.ts
…
$ wc -l src/*.ts
[not run: the fast terminal does not cover this (unquoted glob). Use the regular shell for it.]
```

## What the engine covers

| Command | Forms |
| --- | --- |
| `cat` | `cat [-n] FILE...` |
| `sed` | `sed -n 'A,Bp'`, `'Np'`, `'A,$p'`, several ranges separated by `;` |
| `head`, `tail` | `-N`, `-n N`, `tail -n +N` |
| `wc` | `-l`, `-w`, `-c` and the default |
| `cut` | `-c LIST`, `-b LIST`, `-d X -f LIST`, `-s` |
| `ls` | plain names, `-1`, `-a`, `-A` |
| `echo` | plain words |
| `grep` | `-r -n -i -v -w -x -c -l -L -o -q -s -H -h -I -a -E -F -e -m -A -B -C`, `--include`, `--exclude`, `--exclude-dir` |
| `cd` | remembered for the rest of the session |
| Pipelines and lists | `a | b`, `a && b`, `a || b`, `a ; b`, `2>&1`, `2>/dev/null` |

Everything else is left to your shell: globs, variables, substitutions, redirections to a
file, background jobs, and any program not in this table. See
[docs/architecture.md](docs/architecture.md) for the reasons.

The table describes command families, not every combination of their arguments. In
particular, `grep -w` accepts only literal ASCII words (letters, digits and `_`), `grep -o`
refuses alternative patterns (including several `-e` patterns), and `--color=always` is
refused. Recursive grep paths ending in `/` are refused because macOS versions disagree on
their printed spelling. Use `src` instead of `src/`. Explicit program paths such as
`/custom/cat`, shell special parameters (`$$`, `$?`, etc.), multiple redirections, and
`2>&1` combining both output and errors are also left to the shell.

Shell comments are recognized outside quotes; quoted/escaped `#` stays literal. Lists
and pipelines are checked before any branch runs, including branches skipped by `&&` or
`||`. `cd` keeps logical navigation through symlinks, including across calls. Its options,
`CDPATH`-dependent destinations and lookup failures are left to the shell.

Locale-sensitive cases are deliberately restricted: native `wc -w` and default `wc`
require ASCII input (`wc -l` and `wc -c` still accept other bytes). Native grep refuses
directional word anchors and alphabetic backslash extensions such as `\<`, `\>`, `\A`,
`\b` and `\w`. Case/word matching and bracket expressions on non-ASCII or NUL-containing data can be
refused when equivalence with the BSD locale rules is not established. Literal Unicode
searches remain available. `grep -m 0` and negated filename classes are also refused.
Filename filters use an iterative matcher with a shared work budget instead of recursive
backtracking; exhausting that budget returns a refusal.

## Limits

- **macOS only.** The engine reproduces the BSD tools that ship with macOS. On Linux the GNU
  tools format some outputs differently; that port is not done.
- **Search and read only.** kiro does nothing for the commands that take most of the wall
  time in real sessions: tests, type-checks, builds.
- **Bounded native reads.** Only regular files up to 8 MiB are read; devices, FIFOs and
  larger files are refused before reading. Each stage accepts at most 16 MiB of file input,
  and each output/error buffer is capped at 2 MiB. Oversized native results are refused,
  never silently truncated. CLI fallback runs the original command in the regular shell;
  these bounds do not apply to that shell.
- **Bounded MCP requests.** At most 128 commands per call, 64 KiB per command and 2 MiB of
  combined response text. An input message over 1 MiB is rejected and the server exits.
  Non-UTF-8 output is refused; `exact: true` disables grouping but errors and exit codes
  remain formatted as text.
- **Bounded scout.** At most 10,000 candidate files, 16 MiB of input and 16 MiB of retained
  content; files over 400,000 bytes are skipped. Answers are capped at 64 KiB of UTF-8 text,
  including notices for partial scans or truncated output. Repository `.gitignore` and
  `.git/info/exclude` apply; global Git ignore settings are deliberately not loaded.
  Unreadable candidates are reported separately from documented exclusions, and are not
  counted as successfully read. An incomplete scan returns CLI exit status 1 or MCP
  `isError: true`, while retaining any partial answer. This status is preserved even with
  a zero-sized answer budget. Long-line previews show a match window and mark omissions.
- **Small evidence.** The historical live comparison is one code base, three search tasks,
  one model and five sessions per cell. No current general speedup is claimed.
- **A text search.** `scout` finds words. Code that does the same thing without the words is
  not listed; on the hardest of the three tasks the agent still missed one location in two
  sessions out of five (four out of five without kiro).
- **Coverage.** The supported subset is deliberately conservative. Other commands still
  need the regular shell; historical coverage figures do not describe the current version.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked --all-targets            # unit, differential and benchmark tests
cargo run --release --locked --example replay -- --fixture
```

Differential matrices cover file boundaries, line endings, C/UTF-8 locales, regex/options,
filename filters, comments, conditional lists and logical directory traversal. Supported
cases must execute natively and match stdout, stderr and exit status; expected refusals
are counted separately, so shell fallback cannot disguise a compatibility failure.

The layout and the design rules are described in [docs/architecture.md](docs/architecture.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
