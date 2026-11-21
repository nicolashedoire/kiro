# Benchmarks

Everything here was measured on one machine (Apple M4 Pro, macOS) and one private TypeScript
monorepo of about 1,100 tracked source files, with Claude Code 2.1 as the agent. It shows a
direction, not a law. The two programs that produced the numbers are in
[`examples/`](../examples) so that anyone can run them on their own sessions and code.

## Where the time and the tokens go

Before building anything, 50 days of recorded agent sessions were replayed (about 94,000 tool
calls, 82,000 round trips with the model):

- **Time.** 65 % is the model thinking and writing, 32 % is commands running. One round trip
  costs about 21 seconds on average, 14 of them in the model.
- **Context.** 67 % of what enters the context is command output, 18 % is file reads. The
  commands that fill it are `sed`, `cat` and `grep`: the agent reads and searches code.
- **Reading chains.** Reading and searching in a row, with no edit in between, is 41 % of all
  round trips. Agents send 1.14 tool calls per round trip: almost always one at a time.
- **Tokens.** In long sessions, more than 98 % of the tokens are the conversation being re-read
  at each round trip.

So the cost of a session is, to a first approximation, the number of round trips multiplied
by the size of the context. Making one command faster does not move that. Making the agent
need fewer round trips does.

## Replay: same output, fewer processes

`examples/replay.rs` takes the read/search command lines that agents really ran, keeps those
that are read-only by construction, and runs each one through the regular shell and through
the kiro MCP server.

Run of 2026-11-21, commands recorded on the monorepo, sent one by one over MCP:

| | |
| --- | ---: |
| Shell commands in the recorded sessions | 68,714 |
| Read/search command lines, safe to replay | 7,011 (10.2 %) |
| Sampled / replayed (the rest ran in directories that no longer exist) | 900 / 394 |
| Taken by the engine | 321 (81.5 %) |
| … identical to the shell, byte for byte | 320 |
| … different | 1 |
| Left to the shell | 73 (18.5 %) |

The one difference is the wording of an error: for `cd` into a directory that no longer
exists, zsh prints `zsh:cd:1: no such file or directory: …` and the engine prints
`cd: no such file or directory: …`. Exit codes matched in every case.

What was left to the shell: unquoted globs (34), `echo` with an option (13), `ls` with
several operands or `-l` (12), `tr`, `sort`, `tail -c`, `cat -v` and other forms not
reproduced (14).

Over the 320 commands answered identically:

| | Regular shell | With RTK 0.39 | kiro |
| --- | ---: | ---: | ---: |
| Programs started | 904 | 1,224 | 0 |
| Median time per command | 8.04 ms | 23.86 ms | 0.22 ms |
| Total time | 9,682 ms | 15,190 ms | 4,468 ms |
| Output characters | 2,182,375 | 2,094,172 | 2,181,129 |

Reading the table:

- The median command is 37 times faster than the shell. The total is 2.2 times faster: a few
  large recursive searches dominate it, and their cost is reading the files, which no engine
  avoids.
- [RTK](https://github.com/rtk-ai/rtk) has a different goal: it shrinks outputs. It is in the
  table because it is the tool these sessions were already using. In this replay it rewrote
  219 of the 320 commands, cut the output by 4 %, and added processes: one for its hook and
  one for each wrapper.
- kiro returns the same output as the shell, so it saves almost no characters by itself.
  Grouping search listings by file is lossless but only helps listings that span several
  files, which are rare here.

A caveat on scope: 15 to 45 ms saved per command is small next to the seconds a command
costs inside a real session, where the model and the harness dominate. The in-process engine
matters because it makes batching and one-call exploration cheap and safe, which is what the
next section measures.

## Live: real sessions, with and without kiro

`examples/live.rs` runs the same task several times with a real Claude Code session,
alternating two setups: Claude Code as configured on the machine, and the same plus the kiro
MCP server. File edits are disabled, sessions are not saved, and an answer counts as complete
only if it names every expected location.

Three tasks of the form "list every place in the code to change so that X", five sessions per
task and per setup, model `sonnet`:

| Task | Setup | Complete | Round trips | Time | Tokens | Cost |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| A (3 locations, one of them hidden in a schema) | regular | 1 / 5 | 21.2 | 135 s | 484,800 | $0.35 |
| | kiro | 3 / 5 | 5.2 | 61 s | 140,200 | $0.13 |
| B (3 locations) | regular | 5 / 5 | 14.8 | 225 s | 310,400 | $0.30 |
| | kiro | 5 / 5 | 6.4 | 57 s | 178,300 | $0.19 |
| C (2 locations) | regular | 5 / 5 | 7.4 | 131 s | 130,400 | $0.13 |
| | kiro | 5 / 5 | 4.6 | 26 s | 120,600 | $0.11 |
| **All** | regular | **11 / 15** | **14.5** | **164 s** | **308,500** | **$0.26** |
| | kiro | **13 / 15** | **5.4** | **48 s** | **146,400** | **$0.14** |

That is 2.7 times fewer round trips, 3.4 times less time and 2.1 times fewer tokens, with
answers at least as complete.

### Limits of this comparison

- One code base, one kind of task (searching, no edit), one model.
- Five sessions per cell. Sessions vary a lot: on task A the regular setup took from 71 to
  166 seconds. For task A the five sessions come from two batches run a few hours apart.
- These sessions were measured on the commit just before the public release, when the
  binary and the MCP server had a different name. The code paths are the same.
- Task A stays hard for both setups: one of the three locations does not contain the words
  an agent searches for.

## What was measured and did not work

These ideas looked good and were dropped after measuring them on the recorded sessions:

| Idea | Result |
| --- | --- |
| Return the enclosing block around each `grep` match, so the agent does not read the file afterwards | 8 to 33 % more characters, and only 5 to 6 % of the following reads avoided. After a search, agents read large passages (about 9,000 characters), not the neighbourhood of the match. |
| Show only what changed since the previous run of the same command | Re-runs of the same command after an edit are 2 % of commands and 0.8 % of output; only 14.5 % of that output was already there. |
| Do not show again what is already in the context | 8.6 % of what agents read, 4.8 % of the context, at best. |
| Start tests and type-checks as soon as the code changes, before the agent asks | 0.6 hour removed out of 175 hours of commands: agents rarely repeat exactly the same check. |
| Be notified when a background program ends, instead of polling in a loop | 1.2 hour removed out of 42 hours of polling loops: the rest is the real duration of the programs. |
| Tell the agent more firmly to batch its commands | No effect: 9.3 round trips instead of 10.0. Agents explore step by step because each search depends on the previous one. This is what led to `scout`. |

One measurement pointed the other way and is not addressed yet: 53 % of the code a session
reads had already been read by an earlier session on the same project.

## Reproducing

```sh
# Replay your own recorded sessions (read-only; nothing is modified).
cargo run --release --example replay -- --max 600
cargo run --release --example replay -- --project ~/code/app --mcp target/release/kiro

# Live comparison (this spends real model calls: about $0.15 to $0.35 per session).
echo "List every place in the code to change so that …" > task.txt
cargo run --release --example live -- ~/code/app target/release/kiro out 5 "file-a.ts,file-b.ts" < task.txt
```
