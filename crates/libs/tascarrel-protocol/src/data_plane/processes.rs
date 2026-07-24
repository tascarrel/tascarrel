//! Process execution requests and stream values.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use super::PodId;

/// Maximum data payload expected in a single interactive I/O frame.
pub const MAX_IO_CHUNK_LEN: usize = 64 * 1024;

/// Describes a command. An empty `argv` starts the pod user's login shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    pub pod_id: PodId,
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    /// `Some` requests a pseudo-terminal of the given initial size.
    #[serde(default)]
    pub terminal: Option<TerminalSize>,
}

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

/// The origin of an output chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
    Terminal,
}

/// A portable subset of signals accepted by an execution stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    Hangup,
    Interrupt,
    Terminate,
    Kill,
}

/// The final status of a process. Exactly one field is normally present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}
