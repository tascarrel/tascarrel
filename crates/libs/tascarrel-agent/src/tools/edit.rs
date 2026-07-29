//! Exact text edits guarded by observed revisions.

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
use crate::file_workspace::TextEdit;
use crate::file_workspace::TextFileEdit;

/// Applies one or more exact, non-overlapping text replacements.
#[derive(Clone, Copy, Debug, Default)]
pub struct EditTool;

impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".to_owned(),
            description: "Edit previously read UTF-8 files with exact text replacements. Every oldText must occur exactly once in the original file and must have been returned by read. Edits must not overlap, every file must still match its last read revision, and the complete multi-file batch is validated before any file changes.".to_owned(),
            input_schema: r#"{"type":"object","properties":{"files":{"type":"array","minItems":1,"items":{"type":"object","properties":{"path":{"type":"string","description":"Absolute file path, or path relative to the workspace"},"edits":{"type":"array","minItems":1,"items":{"type":"object","properties":{"oldText":{"type":"string","minLength":1,"description":"Exact, uniquely matching text from a prior read"},"newText":{"type":"string","description":"Replacement text"}},"required":["oldText","newText"],"additionalProperties":false}}},"required":["path","edits"],"additionalProperties":false}}},"required":["files"],"additionalProperties":false}"#.to_owned(),
            prompt: crate::ToolPrompt {
                summary: "Apply exact, revision-safe edits to observed files".to_owned(),
                guidelines: vec![
                    "Use edit for localized changes and write only for new files or complete rewrites."
                        .to_owned(),
                    "Keep oldText as small as possible while still matching exactly once.".to_owned(),
                    "Put multiple changes to the same file in one edit call, without joining distant changes through large unchanged regions."
                        .to_owned(),
                    "All oldText values refer to the original file and must not overlap.".to_owned(),
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
            let input: EditInput = super::parse_arguments("edit", &arguments)?;
            let file_count = input.files.len();
            let edit_count = input
                .files
                .iter()
                .map(|file| file.edits.len())
                .sum::<usize>();
            let edits = input
                .files
                .into_iter()
                .map(|file| TextFileEdit {
                    path: file.path,
                    edits: file
                        .edits
                        .into_iter()
                        .map(|edit| TextEdit {
                            old_text: edit.old_text,
                            new_text: edit.new_text,
                        })
                        .collect(),
                })
                .collect();
            let changes = context
                .files
                .edit_text(edits, &context.cancellation)
                .await?;
            Ok(ToolOutput {
                content: format!("Applied {edit_count} edit(s) across {file_count} file(s)."),
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
struct EditInput {
    files: Vec<EditFileInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EditFileInput {
    path: PathBuf,
    edits: Vec<EditOperationInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EditOperationInput {
    old_text: String,
    new_text: String,
}
