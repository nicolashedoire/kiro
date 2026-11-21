//! kiro: a terminal engine for AI coding agents.
//!
//! An agent that works in a terminal pays for every command twice: the command travels
//! through a chain of processes (hooks, a fresh shell, wrappers, the program itself), and
//! every call is a full round trip with the model that re-reads the whole conversation.
//!
//! kiro keeps the commands the agent already writes and removes what sits behind them:
//!
//! - [`engine`] runs reading and searching commands (`cat`, `sed -n`, `head`, `tail`, `wc`,
//!   `cut`, `ls`, `grep`, pipelines and `;` / `&&` / `||` lists) in-process, with no shell
//!   and no program started, and returns what the system tools would return;
//! - [`scout`] maps an area of the code in one call instead of a dozen searches;
//! - [`mcp`] serves both to an agent over the Model Context Protocol.
//!
//! Anything the engine does not cover is refused or handed to the regular shell, never
//! approximated: one more hop is better than a different result.

mod builtins;
pub mod engine;
pub mod grep;
mod limits;
pub mod mcp;
pub mod scout;
pub mod shell;
