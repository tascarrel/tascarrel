//! Complete UTF-8 file writes guarded by observed revisions.

use std::path::PathBuf;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use serde::Deserialize;

use crate::Tool;
use crate::ToolArtifact;
use crate::ToolContext;
use crate::ToolDefinition;
use crate::ToolOutput;
use crate::ToolResult;

/// Creates a text file or replaces a previously read text file.
#[derive(Clone, Copy, Debug, Default)]
pub struct WriteTool;

impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".to_owned(),
            description: "Write complete UTF-8 file content, creating parent directories for a new file. A new file is created only when the path remains absent. An existing file must have been read completely and must still match the last read revision.".to_owned(),
            input_schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"Absolute file path, or path relative to the workspace"},"content":{"type":"string","description":"Complete replacement file content"}},"required":["path","content"],"additionalProperties":false}"#.to_owned(),
            prompt: crate::ToolPrompt {
                summary: "Create files or completely rewrite observed files".to_owned(),
                guidelines: vec![
                    "Use write only for new files or intentional complete rewrites.".to_owned(),
                    "Read every line of an existing file before rewriting it with write.".to_owned(),
                ],
            },
        }
    }

    fn execute(
        &self,
        context: ToolContext,
        arguments: String,
    ) -> BoxFuture<'static, ToolResult<ToolOutput>> {
        async move {
            let input: WriteInput = super::parse_arguments("write", &arguments)?;
            let byte_count = input.content.len();
            let changes = context
                .files
                .write_text(&input.path, input.content, &context.cancellation)
                .await?;
            Ok(ToolOutput {
                content: format!("Wrote {} ({byte_count} bytes).", input.path.display()),
                artifacts: (!changes.is_empty())
                    .then_some(ToolArtifact::FileChanges { changes })
                    .into_iter()
                    .collect(),
            })
        }
        .boxed()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    path: PathBuf,
    content: String,
}
