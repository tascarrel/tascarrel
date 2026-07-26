//! Built-in revision-aware file tools.

mod bash;
mod edit;
mod process;
mod read;
mod write;

pub use bash::BashTool;
pub use bash::DEFAULT_BASH_OUTPUT_LINE_LIMIT;
pub use bash::DEFAULT_BASH_TIMEOUT;
pub use edit::EditTool;
pub use process::DEFAULT_PROCESS_WAIT;
pub use process::ProcessTool;
pub use read::DEFAULT_READ_BYTE_LIMIT;
pub use read::DEFAULT_READ_LINE_LIMIT;
pub use read::ReadTool;
use reportify::Report;
use serde::de::DeserializeOwned;
pub use write::WriteTool;

use crate::ToolError;
use crate::ToolResult;

fn parse_arguments<T>(tool: &str, arguments: &str) -> ToolResult<T>
where
    T: DeserializeOwned,
{
    serde_json::from_str(arguments).map_err(|source| {
        Report::new(ToolError::InvalidArguments {
            tool: tool.to_owned(),
            message: source.to_string(),
        })
    })
}
