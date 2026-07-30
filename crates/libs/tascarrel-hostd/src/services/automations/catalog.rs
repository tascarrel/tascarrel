//! Strict loading and validation of workspace Automation YAML files.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::str::FromStr as _;

use croner::Cron;
use reportify::Report;
use reportify::ResultExt as _;
use serde::Deserialize;
use tascarrel_api::types::automations as api;
use tascarrel_api::types::chats;

reportify::new_whatever_type! {
    /// Failure while inspecting an Automation catalog.
    pub CatalogError
}

/// Loads every independent definition below `workspace/automations`.
pub fn load(workspace: &Path) -> Result<api::AutomationCatalog, Report<CatalogError>> {
    let directory = workspace.join("automations");
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(empty_catalog());
        }
        Err(error) => {
            return Err(error)
                .whatever("unable to inspect the Automation directory")
                .field("path", directory.display().to_string());
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid("the Automation path must be a real directory")
            .field("path", directory.display().to_string()));
    }

    let mut paths = fs::read_dir(&directory)
        .whatever("unable to read the Automation directory")?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .whatever("unable to inspect an Automation directory entry")
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| {
        matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("yaml" | "yml")
        )
    });
    paths.sort();

    let mut automations = Vec::new();
    let mut errors = Vec::new();
    if paths.len() > MAX_AUTOMATION_FILES {
        errors.push(api::AutomationConfigurationError {
            path: "automations".into(),
            message: format!("at most {MAX_AUTOMATION_FILES} Automation files are accepted").into(),
        });
        paths.truncate(MAX_AUTOMATION_FILES);
    }
    for path in paths {
        let relative = relative_path(workspace, &path);
        match load_file(&path) {
            Ok(definition) => automations.push(definition),
            Err(error) => errors.push(api::AutomationConfigurationError {
                path: relative.into(),
                message: error.to_string().into(),
            }),
        }
    }
    automations.sort_by(|left, right| left.id.cmp(&right.id));
    errors.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(api::AutomationCatalog {
        automations: automations.into(),
        errors: errors.into(),
    })
}

const MAX_AUTOMATION_FILES: usize = 128;
const MAX_AUTOMATION_BYTES: u64 = 1024 * 1024;
const MAX_STEPS: usize = 128;
const MAX_DISPLAY_NAME_BYTES: usize = 256;
const MAX_CONCURRENT: u32 = 64;
const MAX_TIMEOUT_MINUTES: u64 = 365 * 24 * 60;
const DEFAULT_MAX_CONCURRENT: u32 = 1;

fn load_file(path: &Path) -> Result<api::AutomationDefinition, Report<CatalogError>> {
    let metadata = fs::symlink_metadata(path)
        .whatever("unable to inspect the Automation definition")
        .field("path", path.display().to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid("the Automation definition must be a regular file"));
    }
    if metadata.len() > MAX_AUTOMATION_BYTES {
        return Err(invalid(format!(
            "the Automation definition exceeds {MAX_AUTOMATION_BYTES} bytes"
        )));
    }
    let source = fs::read_to_string(path)
        .whatever("unable to read the Automation definition")
        .field("path", path.display().to_string())?;
    let raw: RawAutomation =
        serde_yaml_ng::from_str(&source).whatever("unable to decode the Automation YAML")?;
    let id = definition_id(path)?;
    raw.validate_and_convert(id)
}

fn definition_id(path: &Path) -> Result<String, Report<CatalogError>> {
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| invalid("the Automation filename is not UTF-8"))?;
    validate_identifier(id, "Automation")?;
    Ok(id.to_owned())
}

fn validate_identifier(value: &str, kind: &str) -> Result<(), Report<CatalogError>> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err(invalid(format!(
            "{kind} identifiers must be 1-64 ASCII letters, digits, '-' or '_', and start with a letter or digit"
        )))
    }
}

fn relative_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn empty_catalog() -> api::AutomationCatalog {
    api::AutomationCatalog {
        automations: Vec::new().into(),
        errors: Vec::new().into(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawAutomation {
    name: String,
    description: Option<String>,
    #[serde(rename = "on")]
    triggers: RawTriggers,
    #[serde(default)]
    defaults: RawDefaults,
    #[serde(default)]
    concurrency: RawConcurrency,
    timeout_minutes: Option<u64>,
    steps: Vec<RawStep>,
}

impl RawAutomation {
    fn validate_and_convert(
        self,
        id: String,
    ) -> Result<api::AutomationDefinition, Report<CatalogError>> {
        validate_display_name(&self.name, "Automation name")?;
        if self.steps.is_empty() {
            return Err(invalid("an Automation must contain at least one step"));
        }
        if self.steps.len() > MAX_STEPS {
            return Err(invalid(format!(
                "an Automation may contain at most {MAX_STEPS} steps"
            )));
        }
        if !(1..=MAX_CONCURRENT).contains(&self.concurrency.limit) {
            return Err(invalid(format!(
                "concurrency.limit must be between 1 and {MAX_CONCURRENT}"
            )));
        }
        if self
            .timeout_minutes
            .is_some_and(|minutes| !(1..=MAX_TIMEOUT_MINUTES).contains(&minutes))
        {
            return Err(invalid(format!(
                "timeout-minutes must be between 1 and {MAX_TIMEOUT_MINUTES}"
            )));
        }
        let timeout_seconds = self
            .timeout_minutes
            .map(|minutes| {
                minutes
                    .checked_mul(60)
                    .ok_or_else(|| invalid("timeout-minutes does not fit in seconds"))
            })
            .transpose()?;
        let triggers = self.triggers.convert()?;
        if triggers.is_empty() {
            return Err(invalid(
                "the 'on' mapping must enable workflow-dispatch or contain a schedule",
            ));
        }
        let defaults = self
            .defaults
            .agent
            .map(RawAgentSelection::convert_required)
            .transpose()?;
        let mut step_ids = HashSet::new();
        let mut chat_harness = None;
        let mut steps = Vec::with_capacity(self.steps.len());
        for (index, step) in self.steps.into_iter().enumerate() {
            let definition = step.convert(index, defaults.as_ref())?;
            if !step_ids.insert(definition.id.to_string()) {
                return Err(invalid(format!(
                    "step identifier {:?} is duplicated",
                    definition.id
                )));
            }
            if let api::AutomationStepKind::Agent(agent) = &definition.kind {
                let selection = agent
                    .selection
                    .as_ref()
                    .ok_or_else(|| invalid("an agent step has no validated harness selection"))?;
                if let Some(existing) = &chat_harness {
                    if existing != &selection.harness {
                        return Err(invalid(format!(
                            "agent step {:?} selects a different harness; all agent steps in an Automation share one chat and must use the same harness",
                            definition.id
                        )));
                    }
                } else {
                    chat_harness = Some(selection.harness.clone());
                }
            }
            steps.push(definition);
        }
        Ok(api::AutomationDefinition {
            id: id.into(),
            name: self.name.into(),
            description: self.description.map(Into::into),
            triggers: triggers.into(),
            agent_defaults: defaults,
            max_concurrent: self.concurrency.limit,
            timeout_seconds,
            steps: steps.into(),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawTriggers {
    #[serde(rename = "workflow_dispatch")]
    #[serde(default)]
    workflow_dispatch: Option<RawEmpty>,
    #[serde(default)]
    schedule: Vec<RawSchedule>,
}

impl RawTriggers {
    fn convert(self) -> Result<Vec<api::AutomationTrigger>, Report<CatalogError>> {
        let mut triggers = Vec::new();
        if self.workflow_dispatch.is_some() {
            triggers.push(api::AutomationTrigger::Manual);
        }
        for schedule in self.schedule {
            if schedule.cron.split_ascii_whitespace().count() != 5 {
                return Err(invalid(format!(
                    "schedule cron {:?} must use exactly five fields",
                    schedule.cron
                )));
            }
            Cron::from_str(&schedule.cron).map_err(|error| {
                invalid(format!(
                    "schedule cron {:?} is invalid: {error}",
                    schedule.cron
                ))
            })?;
            triggers.push(api::AutomationTrigger::Schedule(
                api::AutomationScheduleTrigger {
                    cron: schedule.cron.into(),
                },
            ));
        }
        Ok(triggers)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEmpty {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchedule {
    cron: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDefaults {
    agent: Option<RawAgentSelection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConcurrency {
    #[serde(default = "default_max_concurrent")]
    limit: u32,
}

impl Default for RawConcurrency {
    fn default() -> Self {
        Self {
            limit: DEFAULT_MAX_CONCURRENT,
        }
    }
}

const fn default_max_concurrent() -> u32 {
    DEFAULT_MAX_CONCURRENT
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawAgentSelection {
    harness: RawHarness,
    model: Option<String>,
    #[serde(default)]
    options: HashMap<String, RawModelOption>,
}

impl RawAgentSelection {
    fn convert_required(self) -> Result<api::AutomationAgentSelection, Report<CatalogError>> {
        if let Some(model) = &self.model {
            require_nonempty(model, "agent model")?;
        }
        let mut options = self
            .options
            .into_iter()
            .map(|(id, value)| {
                require_nonempty(&id, "agent model option identifier")?;
                Ok(chats::ChatModelOptionSelection {
                    id: id.into(),
                    value: value.convert(),
                })
            })
            .collect::<Result<Vec<_>, Report<CatalogError>>>()?;
        options.sort_by(|left, right| left.id.cmp(&right.id));
        let has_options = !options.is_empty();
        let model = self.model.map(|model| chats::ChatModelSelection {
            model: model.into(),
            options: options.into(),
        });
        if model.is_none() && has_options {
            return Err(invalid("agent model options require an explicit model"));
        }
        Ok(api::AutomationAgentSelection {
            harness: self.harness.convert(),
            model,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RawHarness {
    Tasci,
    Codex,
    ClaudeCode,
}

impl RawHarness {
    const fn convert(self) -> chats::ChatHarnessKind {
        match self {
            Self::Tasci => chats::ChatHarnessKind::Tasci,
            Self::Codex => chats::ChatHarnessKind::Codex,
            Self::ClaudeCode => chats::ChatHarnessKind::ClaudeCode,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum RawModelOption {
    String(String),
    Boolean(bool),
}

impl RawModelOption {
    fn convert(self) -> chats::ChatModelOptionValue {
        match self {
            Self::String(value) => chats::ChatModelOptionValue::String(value.into()),
            Self::Boolean(value) => chats::ChatModelOptionValue::Boolean(value),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawStep {
    id: Option<String>,
    name: Option<String>,
    #[serde(default)]
    continue_on_error: bool,
    run: Option<String>,
    agent: Option<RawAgentStep>,
    host_command: Option<RawHostCommandStep>,
    approval: Option<RawApprovalStep>,
    working_directory: Option<String>,
    #[serde(default)]
    environment: HashMap<String, String>,
}

impl RawStep {
    fn convert(
        self,
        index: usize,
        defaults: Option<&api::AutomationAgentSelection>,
    ) -> Result<api::AutomationStepDefinition, Report<CatalogError>> {
        let operation_count = usize::from(self.run.is_some())
            + usize::from(self.agent.is_some())
            + usize::from(self.host_command.is_some())
            + usize::from(self.approval.is_some());
        if operation_count != 1 {
            return Err(invalid(format!(
                "step {} must define exactly one of run, agent, host-command, or approval",
                index + 1
            )));
        }
        let id = self.id.unwrap_or_else(|| format!("step-{}", index + 1));
        validate_identifier(&id, "step")?;
        let name = self.name.unwrap_or_else(|| id.clone());
        validate_display_name(&name, "step name")?;

        let kind = if let Some(run) = self.run {
            require_nonempty(&run, "run command")?;
            validate_environment(&self.environment)?;
            api::AutomationStepKind::Command(api::AutomationCommandStep {
                run: run.into(),
                working_directory: self.working_directory.map(Into::into),
                environment: convert_map(self.environment),
            })
        } else {
            if self.working_directory.is_some() || !self.environment.is_empty() {
                return Err(invalid(
                    "working-directory and environment are valid only on run steps",
                ));
            }
            if let Some(agent) = self.agent {
                require_nonempty(&agent.prompt, "agent prompt")?;
                let selection = agent
                    .selection
                    .map(RawAgentSelection::convert_required)
                    .transpose()?
                    .or_else(|| defaults.cloned());
                if selection.is_none() {
                    return Err(invalid(format!(
                        "agent step {id:?} requires a harness selection on the step or in defaults.agent"
                    )));
                }
                api::AutomationStepKind::Agent(api::AutomationAgentStep {
                    prompt: agent.prompt.into(),
                    selection,
                })
            } else if let Some(host_command) = self.host_command {
                require_nonempty(&host_command.command, "host-command name")?;
                api::AutomationStepKind::HostCommand(api::AutomationHostCommandStep {
                    command: host_command.command.into(),
                    parameters: convert_map(host_command.parameters),
                })
            } else {
                let approval = self
                    .approval
                    .ok_or_else(|| invalid("the validated step has no operation"))?;
                require_nonempty(&approval.prompt, "approval prompt")?;
                api::AutomationStepKind::Approval(api::AutomationApprovalStep {
                    prompt: approval.prompt.into(),
                })
            }
        };
        Ok(api::AutomationStepDefinition {
            id: id.into(),
            name: name.into(),
            continue_on_error: self.continue_on_error,
            kind,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgentStep {
    prompt: String,
    #[serde(flatten)]
    selection: Option<RawAgentSelection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHostCommandStep {
    command: String,
    #[serde(default)]
    parameters: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApprovalStep {
    prompt: String,
}

fn require_nonempty(value: &str, field: &str) -> Result<(), Report<CatalogError>> {
    if value.trim().is_empty() {
        Err(invalid(format!("{field} must not be empty")))
    } else {
        Ok(())
    }
}

fn validate_display_name(value: &str, field: &str) -> Result<(), Report<CatalogError>> {
    require_nonempty(value, field)?;
    if value.len() > MAX_DISPLAY_NAME_BYTES || value.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{field} must contain at most {MAX_DISPLAY_NAME_BYTES} bytes without control characters"
        )));
    }
    Ok(())
}

fn validate_environment(environment: &HashMap<String, String>) -> Result<(), Report<CatalogError>> {
    for name in environment.keys() {
        if name == "HOME" || name.starts_with("TASCARREL_AUTOMATION_") {
            return Err(invalid(format!(
                "environment variable name {name:?} is reserved by the Automation runner"
            )));
        }
        let valid = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
            && name
                .as_bytes()
                .first()
                .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphabetic());
        if !valid {
            return Err(invalid(format!(
                "environment variable name {name:?} is invalid"
            )));
        }
    }
    Ok(())
}

fn convert_map(
    values: HashMap<String, String>,
) -> HashMap<tascarrel_api::ArcStr, tascarrel_api::ArcStr> {
    values
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
}

fn invalid(message: impl Into<String>) -> Report<CatalogError> {
    Report::whatever(message.into())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tascarrel_api::types::automations::AutomationStepKind;
    use tascarrel_api::types::automations::AutomationTrigger;

    use super::load;

    /// A representative workflow is decoded into the public execution model.
    #[test]
    fn loads_github_actions_shaped_workflow() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join("automations")).unwrap();
        fs::write(
            temporary.path().join("automations/deploy.yaml"),
            r#"
name: Deploy
on:
  workflow_dispatch: {}
  schedule:
    - cron: "15 4 * * 1"
defaults:
  agent:
    harness: codex
    model: gpt-5
concurrency:
  limit: 1
timeout-minutes: 90
steps:
  - id: update
    name: Update lock file
    run: nix flake update
  - id: review
    agent:
      prompt: Review the update and fix regressions.
  - id: rollout
    host-command:
      command: deploy-fleet
      parameters:
        environment: production
"#,
        )
        .unwrap();

        let catalog = load(temporary.path()).unwrap();
        assert!(catalog.errors.is_empty());
        let definition = &catalog.automations[0];
        assert_eq!(definition.id.as_ref(), "deploy");
        assert_eq!(definition.timeout_seconds, Some(5_400));
        assert!(matches!(definition.triggers[0], AutomationTrigger::Manual));
        assert!(matches!(
            definition.steps[1].kind,
            AutomationStepKind::Agent(_)
        ));
    }

    /// One invalid file does not suppress independent valid definitions.
    #[test]
    fn reports_one_bad_file_without_hiding_valid_definitions() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join("automations")).unwrap();
        fs::write(
            temporary.path().join("automations/good.yaml"),
            "name: Good\non:\n  workflow_dispatch: {}\nsteps:\n  - run: echo true\n",
        )
        .unwrap();
        fs::write(
            temporary.path().join("automations/bad.yaml"),
            "name: Bad\non:\n  workflow_dispatch: {}\nsteps:\n  - run: echo false\n    environment:\n      HOME: /tmp\n",
        )
        .unwrap();

        let catalog = load(temporary.path()).unwrap();
        assert_eq!(catalog.automations.len(), 1);
        assert_eq!(catalog.errors.len(), 1);
        assert_eq!(catalog.errors[0].path.as_ref(), "automations/bad.yaml");
        assert!(catalog.errors[0].message.contains("is reserved"));
    }

    /// One Automation-owned chat cannot switch harness implementations.
    #[test]
    fn rejects_agent_steps_with_different_harnesses() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join("automations")).unwrap();
        fs::write(
            temporary.path().join("automations/review.yaml"),
            r"
name: Review
on:
  workflow_dispatch: {}
steps:
  - agent:
      prompt: Inspect the changes.
      harness: codex
  - agent:
      prompt: Apply the fix.
      harness: claude-code
",
        )
        .unwrap();

        let catalog = load(temporary.path()).unwrap();
        assert!(catalog.automations.is_empty());
        assert_eq!(catalog.errors.len(), 1);
        assert!(
            catalog.errors[0]
                .message
                .contains("must use the same harness")
        );
    }
}
