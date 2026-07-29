//! Validation and expansion of immutable host-operation execution plans.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use regex::Regex;
use reportify::Report;
use tascarrel_api::types::config;
use tascarrel_api::types::host_operations as api;

use super::HostOperationServiceError;
use super::StoredEnvironment;
use super::StoredOperation;
use super::invalid_configuration;
use super::invalid_request;

pub(crate) fn validate_command(
    name: &str,
    command: &config::WorkspaceHostCommandConfig,
    workspace: &config::WorkspaceConfig,
) -> Result<(), Report<HostOperationServiceError>> {
    validate_name(name, "command")?;
    if command.program.is_empty() || command.program.contains('\0') {
        return Err(invalid_configuration("command program is empty or invalid"));
    }
    if command.timeout_seconds == Some(0) {
        return Err(invalid_configuration(
            "host command timeout must be greater than zero",
        ));
    }
    if let Some(arguments) = &command.arguments {
        for argument in arguments {
            if argument.contains('\0') {
                return Err(invalid_configuration("host command argument contains NUL"));
            }
        }
    }
    if command
        .working_directory
        .as_deref()
        .is_some_and(|directory| directory.contains('\0'))
    {
        return Err(invalid_configuration(
            "host command working directory contains NUL",
        ));
    }
    if let Some(parameters) = &command.parameters {
        for (name, parameter) in parameters {
            validate_name(name, "parameter")?;
            if parameter.required == Some(true) && parameter.default.is_some() {
                return Err(invalid_configuration(format!(
                    "parameter `{name}` is required and also defines a default"
                )));
            }
            if parameter
                .allowed_values
                .as_ref()
                .is_some_and(|values| values.is_empty())
            {
                return Err(invalid_configuration(format!(
                    "parameter `{name}` has an empty allowed-values list"
                )));
            }
            if let Some(pattern) = &parameter.pattern {
                Regex::new(&format!(r"\A(?:{pattern})\z")).map_err(|error| {
                    invalid_configuration(format!(
                        "parameter `{name}` has an invalid pattern: {error}"
                    ))
                })?;
            }
        }
    }
    if let Some(inputs) = &command.inputs {
        let repositories = workspace.repos.as_ref();
        for (name, input) in inputs {
            validate_name(name, "input")?;
            if repositories.is_none_or(|repos| !repos.contains_key(input.repository.as_ref())) {
                return Err(invalid_configuration(format!(
                    "input `{name}` selects an unconfigured repository `{}`",
                    input.repository
                )));
            }
        }
    }
    if let Some(environment) = &command.environment {
        if let Some(names) = &environment.inherit {
            for name in names {
                validate_environment_name(name)?;
            }
        }
        if let Some(values) = &environment.values {
            for (name, value) in values {
                validate_environment_name(name)?;
                if value.contains('\0') {
                    return Err(invalid_configuration(format!(
                        "environment value `{name}` contains NUL"
                    )));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn resolve_parameters(
    command: &config::WorkspaceHostCommandConfig,
    requested: &HashMap<tascarrel_api::ArcStr, tascarrel_api::ArcStr>,
) -> Result<BTreeMap<String, String>, Report<HostOperationServiceError>> {
    let definitions = command.parameters.as_ref();
    for name in requested.keys() {
        if definitions.is_none_or(|values| !values.contains_key(name)) {
            return Err(invalid_request(format!(
                "parameter `{name}` is not declared by this command"
            )));
        }
    }
    let mut resolved = BTreeMap::new();
    for (name, definition) in definitions.into_iter().flatten() {
        let value = requested
            .get(name)
            .map(ToString::to_string)
            .or_else(|| definition.default.as_ref().map(ToString::to_string));
        let Some(value) = value else {
            if definition.required.unwrap_or(true) {
                return Err(invalid_request(format!(
                    "required parameter `{name}` is missing"
                )));
            }
            continue;
        };
        if value.contains('\0') {
            return Err(invalid_request(format!("parameter `{name}` contains NUL")));
        }
        if let Some(allowed) = &definition.allowed_values
            && !allowed.iter().any(|candidate| candidate.as_ref() == value)
        {
            return Err(invalid_request(format!(
                "parameter `{name}` is not one of its allowed values"
            )));
        }
        if let Some(pattern) = &definition.pattern {
            let expression = Regex::new(&format!(r"\A(?:{pattern})\z")).map_err(|error| {
                invalid_configuration(format!(
                    "parameter `{name}` has an invalid pattern: {error}"
                ))
            })?;
            if !expression.is_match(&value) {
                return Err(invalid_request(format!(
                    "parameter `{name}` does not match its configured pattern"
                )));
            }
        }
        resolved.insert(name.to_string(), value);
    }
    Ok(resolved)
}

pub(crate) fn resolve_inputs(
    command: &config::WorkspaceHostCommandConfig,
) -> Result<(Vec<api::HostOperationInput>, BTreeSet<String>), Report<HostOperationServiceError>> {
    let mut inputs = command
        .inputs
        .as_ref()
        .into_iter()
        .flat_map(|inputs| inputs.iter())
        .map(|(name, input)| {
            let capture = match input
                .capture
                .unwrap_or(config::WorkspaceHostCommandCapture::WorkingTree)
            {
                config::WorkspaceHostCommandCapture::WorkingTree => {
                    api::HostOperationCapture::WorkingTree
                }
                config::WorkspaceHostCommandCapture::CleanHead => {
                    api::HostOperationCapture::CleanHead
                }
                config::WorkspaceHostCommandCapture::Commit => api::HostOperationCapture::Commit,
                config::WorkspaceHostCommandCapture::PublishedRef => {
                    api::HostOperationCapture::PublishedRef
                }
            };
            Ok(api::HostOperationInput {
                name: name.clone(),
                repository: input.repository.clone(),
                capture,
                revision: None,
                base_revision: None,
                materialized_path: None,
                change_summary: None,
            })
        })
        .collect::<Result<Vec<_>, Report<HostOperationServiceError>>>()?;
    inputs.sort_by(|left, right| left.name.cmp(&right.name));
    let pending = inputs.iter().map(|input| input.name.to_string()).collect();
    Ok((inputs, pending))
}

pub(crate) fn pending_input_list(stored: &StoredOperation) -> Vec<api::HostOperationPendingInput> {
    stored
        .operation
        .inputs
        .iter()
        .filter(|input| stored.pending_inputs.contains(input.name.as_ref()))
        .map(|input| api::HostOperationPendingInput {
            name: input.name.clone(),
            repository: input.repository.clone(),
            capture: input.capture,
        })
        .collect()
}

pub(crate) fn expand_argument(
    template: &str,
    parameters: &BTreeMap<String, String>,
    inputs: &BTreeMap<String, PathBuf>,
) -> Result<String, Report<HostOperationServiceError>> {
    expand_template(template, parameters, inputs)
        .map(|value| value.unwrap_or_else(|| template.to_owned()))
}

pub(crate) fn expand_working_directory(
    template: &str,
    work_dir: &Path,
    parameters: &BTreeMap<String, String>,
    inputs: &BTreeMap<String, PathBuf>,
) -> Result<PathBuf, Report<HostOperationServiceError>> {
    if let Some(value) = expand_template(template, parameters, inputs)? {
        return Ok(PathBuf::from(value));
    }
    let path = Path::new(template);
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid_configuration(
            "relative host command working directory must contain only normal components",
        ));
    }
    Ok(work_dir.join(path))
}

pub(crate) fn resolve_environment(
    command: &config::WorkspaceHostCommandConfig,
) -> StoredEnvironment {
    let mut environment = StoredEnvironment {
        inherit: BTreeSet::new(),
        values: BTreeMap::new(),
    };
    let Some(definition) = &command.environment else {
        return environment;
    };
    for name in definition.inherit.as_deref().unwrap_or_default() {
        environment.inherit.insert(name.to_string());
    }
    for (name, value) in definition.values.iter().flatten() {
        environment
            .values
            .insert(name.to_string(), value.to_string());
    }
    environment
}

pub(crate) fn resolve_execution_environment(
    plan: &StoredEnvironment,
) -> Result<BTreeMap<String, String>, Report<HostOperationServiceError>> {
    let mut environment = plan.values.clone();
    for name in &plan.inherit {
        if let Some(value) = env::var_os(name) {
            let value = value.into_string().map_err(|_| {
                invalid_configuration(format!(
                    "inherited environment variable `{name}` is not valid UTF-8"
                ))
            })?;
            environment.insert(name.clone(), value);
        }
    }
    Ok(environment)
}

pub(crate) fn resolve_executable(
    program: &str,
) -> Result<PathBuf, Report<HostOperationServiceError>> {
    let path = Path::new(program);
    if path.is_absolute() {
        return executable(path).ok_or_else(|| {
            invalid_configuration(format!("program is not executable: {}", path.display()))
        });
    }
    if path.components().count() != 1 {
        return Err(invalid_configuration(
            "program must be absolute or a bare executable name",
        ));
    }
    let search_path =
        env::var_os("PATH").ok_or_else(|| invalid_configuration("hostd PATH is unavailable"))?;
    env::split_paths(&search_path)
        .map(|directory| directory.join(path))
        .find_map(|candidate| executable(&candidate))
        .ok_or_else(|| invalid_configuration(format!("program `{program}` was not found in PATH")))
}

fn expand_template(
    template: &str,
    parameters: &BTreeMap<String, String>,
    inputs: &BTreeMap<String, PathBuf>,
) -> Result<Option<String>, Report<HostOperationServiceError>> {
    if let Some(name) = template
        .strip_prefix("${parameters.")
        .and_then(|value| value.strip_suffix('}'))
    {
        return parameters.get(name).cloned().map(Some).ok_or_else(|| {
            invalid_request(format!("parameter placeholder `{name}` has no value"))
        });
    }
    if let Some(name) = template
        .strip_prefix("${inputs.")
        .and_then(|value| value.strip_suffix('}'))
    {
        return inputs
            .get(name)
            .map(|path| Some(path.to_string_lossy().into_owned()))
            .ok_or_else(|| {
                invalid_configuration(format!("input placeholder `{name}` is not declared"))
            });
    }
    if template.contains("${") {
        return Err(invalid_configuration(
            "placeholders must occupy an entire argument and use parameters or inputs",
        ));
    }
    Ok(None)
}

fn executable(path: &Path) -> Option<PathBuf> {
    let canonical = match fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "could not resolve host command executable");
            return None;
        }
    };
    let metadata = match fs::metadata(&canonical) {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::debug!(path = %canonical.display(), %error, "could not inspect host command executable");
            return None;
        }
    };
    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(canonical)
}

fn validate_name(name: &str, kind: &str) -> Result<(), Report<HostOperationServiceError>> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid_configuration(format!(
            "{kind} name `{name}` must use 1-128 ASCII letters, digits, hyphens, or underscores"
        )));
    }
    Ok(())
}

fn validate_environment_name(name: &str) -> Result<(), Report<HostOperationServiceError>> {
    let mut bytes = name.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    if !valid_first || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        return Err(invalid_configuration(format!(
            "environment name `{name}` is invalid"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that placeholders cannot be embedded into literal arguments.
    #[test]
    fn placeholders_must_be_whole_arguments() {
        let parameters = BTreeMap::from([("target".to_owned(), "production".to_owned())]);
        assert_eq!(
            expand_argument("${parameters.target}", &parameters, &BTreeMap::new()).unwrap(),
            "production"
        );
        assert!(
            expand_argument(
                "--target=${parameters.target}",
                &parameters,
                &BTreeMap::new()
            )
            .is_err()
        );
    }

    /// Verifies that environment names are valid without shell interpretation.
    #[test]
    fn environment_names_are_shell_independent() {
        assert!(validate_environment_name("SSH_AUTH_SOCK").is_ok());
        assert!(validate_environment_name("BAD-NAME").is_err());
        assert!(validate_environment_name("1BAD").is_err());
    }
}
