# kiro

**A terminal engine for AI coding agents: same commands, no shell, fewer round trips.**

[![CI](https://github.com/nicolashedoire/kiro/actions/workflows/ci.yml/badge.svg)](https://github.com/nicolashedoire/kiro/actions/workflows/ci.yml)

> Status: early. macOS only for now. The numbers below come from one machine and one code
> base; read the [limits](#limits) before relying on them.
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
  shell, no program started, and the output is the one the system tools give, byte for byte.
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
3. **Read-only by construction.** The MCP tools cannot write, delete or start anything, so a
   single permission covers every call.

## Results

**Live, with real Claude Code sessions.** Three "find every place to change" tasks on a private
TypeScript monorepo, five sessions per task and per setup, same model, file edits disabled:

| Mean over 15 sessions | Regular terminal | With kiro |
| --- | ---: | ---: |
| Complete answers | 11 / 15 | 13 / 15 |
| Round trips with the model | 14.5 | 5.4 |
| Time | 164 s | 48 s |
| Tokens | 308,500 | 146,400 |
| Cost per session | $0.26 | $0.14 |

**Replay, on recorded commands.** 394 read/search commands that agents really ran on
that code base, sent one by one to the kiro MCP server and to the regular shell:

| Over the 320 commands answered identically | Regular shell | kiro |
| --- | ---: | ---: |
| Programs started | 904 | 0 |
| Median time per command | 8.04 ms | 0.22 ms |
| Total time | 9,682 ms | 4,468 ms |

The engine took 321 of the 394 commands (81 %) and left 73 to the shell, mostly because of
unquoted globs. 320 of its 321 answers were identical byte for byte; the last one differed
only in the wording of the error for a `cd` into a directory that no longer exists. The
median command is 37 times faster. The total is only 2.2 times faster: a few large recursive
searches are dominated by reading the files, which no engine avoids.

The method, the raw numbers, and the ideas that were measured and **did not** work are in
[docs/benchmarks.md](docs/benchmarks.md).

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
SCOUT - 7 files mention these terms (412 files scanned)
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

## Limits

- **macOS only.** The engine reproduces the BSD tools that ship with macOS. On Linux the GNU
  tools format some outputs differently; that port is not done.
- **Search and read only.** kiro does nothing for the commands that take most of the wall
  time in real sessions: tests, type-checks, builds.
- **Small evidence.** The live comparison is one code base, three search tasks, one model and
  five sessions per cell. Sessions vary a lot from one run to the next.
- **A text search.** `scout` finds words. Code that does the same thing without the words is
  not listed; on the hardest of the three tasks the agent still missed one location in two
  sessions out of five (four out of five without kiro).
- **Coverage.** On the recorded sessions, read/search command lines the engine can take are
  about 10 % of all the shell commands the agents ran. The rest still goes through the shell.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                                  # unit tests + differential tests (macOS)
cargo run --release --example replay -- --max 600
```

The layout and the design rules are described in [docs/architecture.md](docs/architecture.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
