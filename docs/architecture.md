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

kiro removes both kinds of layers while keeping the interface the agent already knows: the
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
| `limits` | Bounded regular-file reads, input and output budgets; exceeding a bound refuses the native command. |
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

Comments are stripped only at an unquoted word boundary, retaining newlines and quoted or
escaped hashes. Every pipeline is parsed before conditional evaluation; malformed syntax
in a skipped branch cannot become a successful result or commit a preceding `cd`.
Native navigation follows logical `cd -L` semantics: symlink names remain in the session
path and `..` removes a logical component. Startup honors `PWD` only if it identifies the
actual current directory; CLI shell fallback receives the same logical `PWD`. Explicit
`cd` options, `CDPATH`-dependent paths and failed directory lookups are refused.

### The rule: never approximate

A command line is either run entirely inside the engine or not at all. As soon as one stage
is not covered, the engine returns `Err(reason)` and keeps nothing, not even a `cd` that came
before in the same line. The caller then decides:

- `kiro run` hands the whole line, untouched, to the regular shell (`KIRO_SHELL`, then
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
| Ambiguous regex output | `grep -Eo 'a\|ab'`, `grep -w @` | Rust regex and BSD/POSIX semantics differ. `-o` alternatives and non-literal `-w` patterns are refused. |
| Explicit program paths | `/custom/cat f`, `/usr/bin/grep x f` | A matching basename does not establish which program the user intended. |
| Mixed or multiple redirections | `cat f missing 2>&1`, `2>&1 2>/dev/null` | Output/error interleaving is not preserved by separate buffers; these cases are refused. |

### Fidelity details

The engine targets the BSD userland that ships with macOS. A few things that turned out to
matter, all covered by tests:

- **`grep -r` order.** The system tool visits files in directory order, not sorted. The
  engine does the same, otherwise `grep -r … | head` would return different lines.
- **`grep -r dir/`.** Some macOS versions print `dir//file`, others `dir/file`. The engine
  refuses this form without starting a probe or writing temporary files. `dir` is supported.
- **Quoted paths and shell parameters.** Quoted or escaped `~` is literal. Only the parser
  expands an unquoted home-directory prefix. Special parameters such as `$$`, `$?` and `$@`
  are refused rather than printed literally.
- **Basic regular expressions.** `\|`, `\(`, `\+`, `\?` are operators; a bare `+ ? | ( ) {`
  is a literal. `$` is an anchor only at the very end or before `\)`: before `\|` the system
  `grep` takes it as a literal dollar sign. A `*` right after a leading `^` is a literal star.
- **Locale.** `LC_ALL`, `LC_CTYPE`, `LANG` select UTF-8 character or byte handling. This
  does not reproduce all BSD collation/classification rules: grep refuses ambiguous
  non-ASCII case/word/bracket matches and directional or alphabetic backslash extensions.
  Literal Unicode searches remain supported. `wc -w` and default `wc` refuse non-ASCII
  data; line and byte counts remain native. Refusals are distinct from zero matches.
- **Missing final newline.** `sed` counts lines across files and terminates a partial line
  when another global input line follows. `cut` processes each file independently; `-f`
  on a line without a delimiter copies it unchanged. These boundaries have generated
  differential coverage, including empty inputs and pipelines.
- **Numeric options.** Unsupported zero, negative or malformed head counts are refused;
  supported signed tail forms are checked against BSD rather than stripping arbitrary signs.
- **Binary files.** `grep` prints `Binary file X matches` and `-I` skips them.
- **Errors.** Messages and exit codes are those of the tools: `cat: f: No such file or
  directory` (1), `grep: f: No such file or directory` (2), `wc: f: open: No such file or
  directory` (1).

Anything outside what has been verified returns `Err` instead.

## The MCP server

One process per session, speaking JSON-RPC over standard input and output. Native `run`
starts no subprocess; `scout` intentionally invokes system Git for file discovery. The
working directory persists between calls. The tools use the server's filesystem permissions
and may read outside the initial project directory: they are not an OS sandbox.

### `run`

Takes `command` or, preferably, `commands` (a list). All outputs come back in one answer:

```
$ grep -n "export function" src/api.ts
12:export function load(id: number) {
$ sed -n '12,30p' src/api.ts
…
```

- **Reading only.** There is no shell fallback in MCP. An unsupported native command returns
  a refusal and does not commit a directory change. The tool carries `readOnlyHint`; it is
  not marked idempotent because a relative `cd` changes persistent state.
- **Grouped listings.** When every line of an output is `path:line:text` for an existing
  file, the path is written once and each line keeps `line:text`. Pass `exact: true` to
  disable grouping. MCP returns UTF-8 text, with stderr and exit codes formatted into the
  response; it does not expose raw binary stdout/stderr streams. Non-UTF-8 output is refused.

### Resource bounds

Native reads require regular files, at most 8 MiB per file and 16 MiB of input per stage.
Known FIFOs and devices are refused before opening; Darwin's nonblocking-open flag also
prevents a FIFO replacement from blocking `open`. Reads remain capped if a file grows.
Output/error buffers and the accumulated command response are capped at 2 MiB each. A bound
returns `Err`, never a successful truncated native result. CLI fallback is the ordinary
shell and is not subject to these native limits.

MCP accepts 128 commands per call, 64 KiB per command, and one JSON message per line up to
1 MiB. An oversized transport message returns an error and ends the server, avoiding an
unbounded read or an attempt to parse the remainder as another message. These are memory
and enumeration bounds, not a wall-clock deadline for all filesystem operations.

Filename filters are compiled once and matched iteratively, without recursive backtracking.
One search shares a budget of two million matching operations across include, exclude and
excluded-directory checks. An exhausted budget refuses the command; it cannot produce a
successful incomplete listing. Ambiguous negated or locale-sensitive filename classes are
also refused.

### `scout`

Takes `terms` (plain text, case-insensitive) and optionally `paths` and `budget`. It answers
"where does this live?" in one call:

1. **Files.** `/usr/bin/git ls-files` lists tracked and untracked, non-ignored files. Git
   hooks, fsmonitor, optional locks and global/system configuration are disabled; local
   `.gitignore` and `.git/info/exclude` remain effective. Discovery has a two-second timeout
   and a 2 MiB listing limit. Outside a repository, a filtered directory walk is used.
   An empty successful Git listing stays empty instead of falling back to ignored files.
   Binary files and files over 400,000 bytes are skipped.
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

Discovery accepts at most 10,000 candidates; directory walking is bounded to 50,000 entries
and depth 64. Input and stored matching content are each capped at 16 MiB; transient
parsing, ranking and rendering allocations are additional. A partial scan is
explicitly marked. The answer budget is a strict UTF-8 byte cap, at most 64 KiB, including
any truncation notice. This limits work as well as presentation.

Successful text reads, documented exclusions and I/O failures are counted separately. An
unreadable candidate marks the scan incomplete, including when there are no matches; it is
not counted as successfully read. `scout_result` returns `Err` for invalid requests or
incomplete scans, keeping any bounded partial text in the error. The legacy `scout`
string wrapper remains available. CLI exit status 1 and MCP `isError: true` expose this
status independently of the answer budget, even when it is zero. Long-line previews center
on a match and mark clipping with ellipses and a request to read the full line.

## Differential coverage

Generated matrices compare supported native forms with the actual macOS tools in both C
and UTF-8 locales, across file boundaries, line endings, regex/options and filename filters.
Shell matrices compare comments, quoted hashes, conditionals and logical directory changes
with both `/bin/sh` and `/bin/zsh`. Nonportable/malformed lists are refused conservatively.
Positive tests require native execution and exact stdout, stderr and exit status; refusals
are counted separately. Subprocess regressions also verify MCP availability after the
formerly exponential glob and truthful Scout error status with an empty output budget.

## Why batching is the point

The intended benefit is fewer model interactions: many independent reads per call, and a
first call that returns a useful map. The public replay fixture checks command equivalence;
historical live-session measurements are archived in [benchmarks.md](benchmarks.md), with
their limitations. They have not been rerun after the audit fixes and do not establish a
current general speedup or isolate the benefit of the engine, batching and scout.
