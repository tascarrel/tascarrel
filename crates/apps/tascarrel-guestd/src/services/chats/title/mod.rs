//! Independent services for deriving concise chat titles from prompts.

mod claude;
mod codex;
mod process;

pub use claude::ClaudeExecTitleGenerator;
pub use codex::CodexExecTitleGenerator;
use futures_util::future::BoxFuture;
use tascarrel_api::types::chats::ChatHarnessKind;
use tascarrel_api::types::chats::ChatPrompt;

/// Maximum number of characters retained in a chat title.
pub const MAX_TITLE_CHARACTERS: usize = 64;

/// Input supplied to an independent title-generation service.
#[derive(Clone, Debug, PartialEq)]
pub struct GenerateTitleRequest {
    /// Harness whose provider should generate the title.
    pub harness: ChatHarnessKind,
    /// Prompt from which the title should be derived.
    pub prompt: ChatPrompt,
}

/// A title returned by a title-generation service.
#[derive(Clone, Debug, PartialEq)]
pub struct GeneratedTitle {
    /// Validated single-line title.
    pub title: String,
}

/// Failure to produce a valid generated title.
#[derive(Clone, Debug, PartialEq)]
pub struct TitleGenerationError {
    /// Stable implementation-defined error code.
    pub code: String,
    /// Human-readable diagnostic that does not contain the source prompt.
    pub message: String,
}

/// Generates chat titles without using or mutating a chat's main harness
/// session.
pub trait TitleGenerationService: Send + Sync {
    /// Generates a concise title for one prompt.
    fn generate_title(
        &self,
        request: GenerateTitleRequest,
    ) -> BoxFuture<'_, Result<GeneratedTitle, TitleGenerationError>>;
}

/// Derives the immediate display title used while generation is in progress or
/// unavailable.
#[must_use]
pub fn fallback_title(prompt: &ChatPrompt) -> String {
    prompt
        .text
        .as_deref()
        .map(normalize_title)
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "New chat".to_owned())
}

fn normalize_title(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = normalized.chars().count();
    if count <= MAX_TITLE_CHARACTERS {
        return normalized;
    }
    let prefix = normalized
        .chars()
        .take(MAX_TITLE_CHARACTERS.saturating_sub(3))
        .collect::<String>();
    format!("{prefix}…")
}

fn validate_generated_title(value: &str) -> Result<GeneratedTitle, TitleGenerationError> {
    let title = normalize_title(value);
    if title.is_empty() {
        return Err(error(
            "invalid_output",
            "the title generator returned an empty title",
        ));
    }
    Ok(GeneratedTitle { title })
}

fn error(code: impl Into<String>, message: impl Into<String>) -> TitleGenerationError {
    TitleGenerationError {
        code: code.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use tascarrel_api::ArcVec;
    use tascarrel_api::types::chats::ChatPrompt;

    use super::fallback_title;
    use super::validate_generated_title;

    /// Confirms fallback titles use the same normalization and bound as
    /// generated titles.
    #[test]
    fn fallback_title_matches_the_frontend_shape() {
        assert_eq!(
            fallback_title(&prompt("  Fix   the\nlogin flow  ")),
            "Fix the login flow"
        );
        assert_eq!(fallback_title(&prompt("")), "New chat");
        assert_eq!(
            fallback_title(&prompt(&"a".repeat(80))),
            format!("{}…", "a".repeat(61))
        );
    }

    /// Confirms generated titles reject empty output and normalize valid
    /// output.
    #[test]
    fn generated_titles_are_normalized_and_bounded() {
        assert_eq!(
            validate_generated_title("  Diagnose   flaky\ntests ")
                .unwrap()
                .title,
            "Diagnose flaky tests"
        );
        assert!(validate_generated_title(" \n ").is_err());
    }

    fn prompt(text: &str) -> ChatPrompt {
        ChatPrompt {
            text: Some(text.into()),
            attachments: ArcVec::new(),
            model: None,
        }
    }
}
