# Architecture

## The problem: two kinds of layers

When an AI coding agent reads one file through a terminal, this is what happens:

1. The model writes a shell command as text, with the shell's quoting rules.
2. The agent harness reviews the command (permission rules, sometimes a remote classifier).
3. Hooks run, each one a new process.
4. A fresh shell starts and loads its profile.
5. Wrapper programs start.
6. The real program runs.
7. Its output, formatted for human eyes, is captured and truncated.

Only step 6 is work. Everything else is a relay.

There is a second, more expensive layer on top: **every tool call is a full round trip with
the model**, and each round trip re-reads the whole conversation. An agent that explores a
code base with one `grep` or one `sed` per call pays that price a dozen times before it has
changed a single line.

itto removes both kinds of layers while keeping the interface the agent already knows: the
commands themselves.

## The pieces

```
            agent
              │  MCP (JSON-RPC over stdio, one long-lived process)
              ▼
        ┌───────────┐
        │   mcp     │  tools: run, scout
        └─────┬─────┘
       ┌──────┴───────┐
       ▼              ▼
  ┌─────────┐    ┌─────────┐
  │ engine  │    │  scout  │
  └────┬────┘    └─────────┘
       │ list → pipeline → stage
       ▼
  ┌──────────┬────────┐
  │ builtins │  grep  │      (in-process: no shell, no program)
  └──────────┴────────┘
```

| Module | Role |
| --- | --- |
| `shell` | Reads a command line cautiously: words, quotes, pipes, `;` / `&&` / `||` lists. Flags every word a shell would expand. |
| `engine` | Runs a command line: splits it into a list of pipelines, runs each stage in-process, keeps the working directory. |
| `builtins` | `cat`, `sed -n`, `head`, `tail`, `wc`, `cut`, `echo`, `ls`, reproduced for the forms agents actually use. |
| `grep` | `grep` with its usual options, on top of the `regex` crate. |
| `scout` | One-call map of an area of the code. |
| `mcp` | The MCP server that exposes `run` and `scout`. |

## The engine

A command line goes through three levels:

1. **List.** `a && b ; c || d` is split outside quotes. `&&` runs the next item if the
   previous one succeeded, `||` if it failed, `;` always.
2. **Pipeline.** `a | b | c` is split outside quotes. The output of a stage is the input of
   the next one, in memory.
3. **Stage.** One command with its arguments, dispatched to a built-in.

`cd` is handled by the engine itself: the working directory is part of the session, so no
shell is needed to remember it between calls.

### The rule: never approximate

A command line is either run entirely inside the engine or not at all. As soon as one stage
is not covered, the engine returns `Err(reason)` and keeps nothing, not even a `cd` that came
before in the same line. The caller then decides:

- `itto run` hands the whole line, untouched, to the regular shell (`ITTO_SHELL`, then
  `SHELL`, then `/bin/sh`);
- the MCP server refuses the command and tells the agent to use its regular shell.

One more hop is better than a different result.

### What is left to the shell, and why

| Construct | Example | Reason |
| --- | --- | --- |
| Unquoted glob | `cat src/*.ts` | Expansion, ordering and the "no match" error belong to the shell. |
| Braces, brackets | `cat {a,b}.ts`, `ls [ab]*` | Same. |
| Leading `=` | `echo =====` | zsh replaces `=name` with the path of a command, and fails if there is none. |
| Variables, substitutions | `$HOME`, `$(pwd)`, backticks | Only the shell knows their values. |
| Redirection to a file | `> out.txt`, `>>`, `<` | The engine is read-only. Only `2>&1` and `2>/dev/null` are handled. |
| Background jobs | `sleep 5 &` | Not a read. |
| `echo` with an option or a backslash | `echo -n x` | Shells disagree: `sh` prints `-n x`, `zsh` prints `x` without a newline. |
| Wrappers | `rtk grep …`, `time grep …` | A different program, with its own output. |
| Options not reproduced | `ls -l`, `sed` without `-n`, `tail -f`, `grep -P` | Not worth guessing. |

### Fidelity details

The engine targets the BSD userland that ships with macOS. A few things that turned out to
matter, all covered by tests:

- **`grep -r` order.** The system tool visits files in directory order, not sorted. The
  engine does the same, otherwise `grep -r … | head` would return different lines.
- **`grep -r dir/`.** The path is printed as the system tool builds it: `dir//file`.
- **Basic regular expressions.** `\|`, `\(`, `\+`, `\?` are operators; a bare `+ ? | ( ) {`
  is a literal. `$` is an anchor only at the very end or before `\)`: before `\|` the system
  `grep` takes it as a literal dollar sign. A `*` right after a leading `^` is a literal star.
- **Locale.** In a UTF-8 locale `grep` and `cut` count characters; otherwise they count
  bytes. `[^']{20,}` does not match the same lines in the two cases. The engine follows
  `LC_ALL`, `LC_CTYPE`, `LANG` like the tools do.
- **Missing final newline.** `sed -n '$p'` and `cut -f` on a line without a delimiter keep a
  last line without its newline; `grep`, `head` and `cut -c` add one.
- **Binary files.** `grep` prints `Binary file X matches` and `-I` skips them.
- **Errors.** Messages and exit codes are those of the tools: `cat: f: No such file or
  directory` (1), `grep: f: No such file or directory` (2), `wc: f: open: No such file or
  directory` (1).

Anything outside what has been verified returns `Err` instead.

## The MCP server

One process per session, speaking JSON-RPC over standard input and output. Because it stays
alive, nothing is started per command and the working directory persists.

### `run`

Takes `command` or, preferably, `commands` (a list). All outputs come back in one answer:

```
$ grep -n "export function" src/api.ts
12:export function load(id: number) {
$ sed -n '12,30p' src/api.ts
…
```

- **Read-only by construction.** There is no fallback here: a command the engine does not
  cover is refused and nothing is executed. One permission therefore covers every call, and
  the tool carries the `readOnlyHint` annotation.
- **Grouped listings.** When every line of an output is `path:line:text` for an existing
  file, the path is written once and each line keeps `line:text`. Nothing is lost; pass
  `exact: true` for the byte-for-byte output.

### `scout`

Takes `terms` (plain text, case-insensitive) and optionally `paths` and `budget`. It answers
"where does this live?" in one call:

1. **Files.** The files git tracks (plus untracked, non-ignored ones); a filtered directory
   walk when there is no repository. Binary and very large files are skipped.
2. **Matches.** One pass per file with a set of all the terms.
3. **Ranking by rarity.** A term that appears on 14 lines says more than one that appears on
   232. A line is worth the rarity of the terms it contains; a file is worth its best lines.
4. **Neighbours.** A file gets a share of the best score of its directory: a schema next to
   the module that matches strongly is probably part of the same feature, even if it only
   mentions a common term.
5. **Rendering.** For each file, the most telling lines in file order, each with the line that
   opens its block (the enclosing function or class) and the rest of the statement when it
   continues on more-indented lines. The remaining files get a two-line preview each, then a
   plain list. The whole answer fits in the budget.

The scout is a text search. It says so in its output: code that does the same thing without
the words is not listed.

## Why batching is the point

Measured on real sessions (see [benchmarks.md](benchmarks.md)), running a command in-process
saves 15 to 45 ms. A round trip with the model costs tens of seconds and re-reads the whole
conversation. The in-process engine matters because it makes a different way of driving the
terminal cheap and safe: many commands per call, and a first call that already returns the
map. That is where the round trips, the time and the tokens go away.
