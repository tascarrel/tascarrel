//! Bounded UTF-8 file reads with revision and coverage recording.

use std::path::PathBuf;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use serde::Deserialize;

use crate::Tool;
use crate::ToolContext;
use crate::ToolDefinition;
use crate::ToolOutput;
use crate::ToolResult;

/// Reads bounded text ranges and records exactly which bytes the model saw.
#[derive(Clone, Copy, Debug)]
pub struct ReadTool {
    line_limit: usize,
    byte_limit: usize,
}

impl ReadTool {
    /// Creates a read tool with caller-controlled output limits.
    #[must_use]
    pub const fn new(line_limit: usize, byte_limit: usize) -> Self {
        Self {
            line_limit,
            byte_limit,
        }
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new(DEFAULT_READ_LINE_LIMIT, DEFAULT_READ_BYTE_LIMIT)
    }
}

impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read".to_owned(),
            description: format!(
                "Read a UTF-8 file with one-based line paging. Output is limited to {} lines and {} bytes per call. Existing text must be returned by read before edit can match it, and write requires the complete current file to have been read.",
                self.line_limit, self.byte_limit
            ),
            input_schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative or in-workspace absolute file path"},"offset":{"type":"integer","minimum":1,"description":"One-based first line; defaults to 1"},"byteOffset":{"type":"integer","minimum":0,"description":"UTF-8 byte offset within the first selected line; used only to continue an overlong line"},"limit":{"type":"integer","minimum":1,"description":"Maximum lines to return; the harness output limit still applies"}},"required":["path"],"additionalProperties":false}"#.to_owned(),
            prompt: crate::ToolPrompt {
                summary: "Read workspace file contents".to_owned(),
                guidelines: vec![
                    "Use read to inspect file contents instead of cat, sed, or shell pipelines."
                        .to_owned(),
                    "Continue paged reads with offset and, when returned, byteOffset until all content needed for a change has been observed."
                        .to_owned(),
                ],
            },
        }
    }

    fn execute(
        &self,
        context: ToolContext,
        arguments: String,
    ) -> BoxFuture<'static, ToolResult<ToolOutput>> {
        let line_limit = self.line_limit;
        let byte_limit = self.byte_limit;
        async move {
            let input: ReadInput = super::parse_arguments("read", &arguments)?;
            let requested_limit = input.limit.unwrap_or(line_limit);
            let read = context
                .files
                .read_text(
                    &input.path,
                    input.offset.unwrap_or(1),
                    input.byte_offset.unwrap_or(0),
                    requested_limit.min(line_limit),
                    byte_limit,
                    &context.cancellation,
                )
                .await?;
            Ok(ToolOutput::text(format_read_output(read)))
        }
        .boxed()
    }
}

/// Default maximum number of lines returned by one read.
pub const DEFAULT_READ_LINE_LIMIT: usize = 2_000;

/// Default maximum number of UTF-8 bytes returned by one read.
pub const DEFAULT_READ_BYTE_LIMIT: usize = 50 * 1_024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadInput {
    path: PathBuf,
    offset: Option<usize>,
    byte_offset: Option<usize>,
    limit: Option<usize>,
}

fn format_read_output(read: crate::file_workspace::TextRead) -> String {
    if read.byte_limited {
        let next_offset = read.next_offset.unwrap_or(read.start_line);
        return format!(
            "{}\n\n[Output limit reached while showing line {} of {}. Use offset={}, byteOffset={} to continue.]",
            read.content, next_offset, read.total_lines, next_offset, read.next_byte_offset
        );
    }
    if let Some(next_offset) = read.next_offset {
        return format!(
            "{}\n\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
            read.content, read.start_line, read.end_line, read.total_lines, next_offset
        );
    }
    read.content
}
