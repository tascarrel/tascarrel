//! Dynamic system prompts assembled from the active tool and project context.

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use reportify::Report;
use tokio::fs;

use crate::AgentConfig;
use crate::AgentError;
use crate::AgentResult;
use crate::ToolDefinition;

/// Builds the model-visible coding-agent contract for one run.
pub(crate) async fn build_system_prompt(
    tools: &[ToolDefinition],
    root: &Path,
    config: &AgentConfig,
) -> AgentResult<String> {
    let mut prompt = String::from(
        "You are Tasci, an expert coding agent operating through the Tascarrel harness.",
    );

    if !tools.is_empty() {
        prompt.push_str("\n\nAvailable tools:\n");
        for tool in tools {
            prompt.push_str("- ");
            prompt.push_str(&tool.name);
            prompt.push_str(": ");
            if tool.prompt.summary.trim().is_empty() {
                prompt.push_str(&tool.description);
            } else {
                prompt.push_str(&tool.prompt.summary);
            }
            prompt.push('\n');
        }
    }

    let mut guidelines = Vec::new();
    let mut seen = HashSet::new();
    for tool in tools {
        for guideline in &tool.prompt.guidelines {
            if seen.insert(guideline.as_str()) {
                guidelines.push(guideline.as_str());
            }
        }
    }
    if tools
        .iter()
        .any(|tool| matches!(tool.name.as_str(), "edit" | "write"))
    {
        let guideline =
            "If a file changed after it was read, read it again before retrying the change.";
        if seen.insert(guideline) {
            guidelines.push(guideline);
        }
    }
    let response_guideline = "Be concise in responses and state changed file paths clearly.";
    if seen.insert(response_guideline) {
        guidelines.push(response_guideline);
    }
    if !guidelines.is_empty() {
        prompt.push_str("\nGuidelines:\n");
        for guideline in guidelines {
            prompt.push_str("- ");
            prompt.push_str(guideline);
            prompt.push('\n');
        }
    }

    prompt.push_str("\nCurrent working directory: ");
    prompt.push_str(&root.display().to_string());

    let instructions = load_project_instructions(root, &config.project_instruction_files).await?;
    if !instructions.is_empty() {
        prompt.push_str("\n\n<project_context>");
        for instruction in instructions {
            prompt.push_str("\n<project_instructions path=\"");
            prompt.push_str(&instruction.path.display().to_string());
            prompt.push_str("\">\n");
            prompt.push_str(&instruction.content);
            if !instruction.content.ends_with('\n') {
                prompt.push('\n');
            }
            prompt.push_str("</project_instructions>");
        }
        prompt.push_str("\n</project_context>");
    }

    if !config.additional_instructions.is_empty() {
        prompt.push_str("\n\nAdditional harness instructions:\n");
        for instruction in &config.additional_instructions {
            prompt.push_str("- ");
            prompt.push_str(instruction);
            prompt.push('\n');
        }
    }

    Ok(prompt)
}

struct ProjectInstruction {
    path: PathBuf,
    content: String,
}

async fn load_project_instructions(
    root: &Path,
    configured_paths: &[PathBuf],
) -> AgentResult<Vec<ProjectInstruction>> {
    let mut instructions = Vec::new();
    for configured_path in configured_paths {
        let requested = if configured_path.is_absolute() {
            configured_path.clone()
        } else {
            root.join(configured_path)
        };
        let canonical = match fs::canonicalize(&requested).await {
            Ok(path) => path,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Report::new(AgentError::ProjectInstructions {
                    path: requested,
                    source,
                }));
            }
        };
        if !canonical.starts_with(root) {
            return Err(Report::new(
                AgentError::ProjectInstructionsOutsideWorkspace { path: canonical },
            ));
        }
        let content = fs::read_to_string(&canonical).await.map_err(|source| {
            Report::new(AgentError::ProjectInstructions {
                path: canonical.clone(),
                source,
            })
        })?;
        let path = canonical
            .strip_prefix(root)
            .unwrap_or(&canonical)
            .to_path_buf();
        instructions.push(ProjectInstruction { path, content });
    }
    Ok(instructions)
}
