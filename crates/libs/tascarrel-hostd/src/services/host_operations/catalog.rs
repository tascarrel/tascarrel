//! Live caller-visible catalog of trusted workspace host commands.
//!
//! [`HostCommandSubscription`] filters watched workspace configuration
//! snapshots into the command metadata which authenticated pods may inspect.

use std::collections::BTreeSet;

use reportify::Report;
use tascarrel_api::types::config as config_api;
use tascarrel_api::types::host_operations as api;

use super::HostOperationService;
use super::HostOperationServiceError;
use super::plan::resolve_capture;
use super::unavailable;
use crate::services::config::ConfigService;
use crate::services::config::ConfigSubscription;

impl HostOperationService {
    /// Opens a live caller-visible catalog of trusted commands registered for
    /// one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace configuration cannot be observed.
    #[tracing::instrument(level = "debug", skip_all, fields(workspace = %input.workspace), err)]
    pub async fn subscribe_commands(
        &self,
        input: api::HostCommandListChangedSubscription,
        config_service: &ConfigService,
    ) -> Result<HostCommandSubscription, Report<HostOperationServiceError>> {
        let subscription = config_service
            .subscribe(config_api::ConfigChangedSubscription {
                workspace_name: input.workspace,
            })
            .await
            .map_err(|report| {
                report.escalate(HostOperationServiceError::Unavailable(
                    "workspace configuration observation failed".to_owned(),
                ))
            })?;
        Ok(HostCommandSubscription {
            config: subscription,
            previous: None,
        })
    }
}

/// Latest-value caller-visible host-command catalog.
pub struct HostCommandSubscription {
    config: ConfigSubscription,
    previous: Option<api::HostCommandListChangedEvent>,
}

impl HostCommandSubscription {
    /// Receives the current catalog and each later changed catalog.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration observation stops.
    pub async fn recv(
        &mut self,
    ) -> Result<api::HostCommandListChangedEvent, Report<HostOperationServiceError>> {
        loop {
            let snapshot = self
                .config
                .recv()
                .await
                .ok_or_else(|| unavailable("configuration observation stopped"))?;
            let event = host_command_catalog(snapshot);
            if self.previous.as_ref() == Some(&event) {
                continue;
            }
            self.previous = Some(event.clone());
            return Ok(event);
        }
    }
}

/// Builds the caller-visible command catalog from one retained configuration
/// snapshot.
fn host_command_catalog(
    snapshot: config_api::ConfigChangedEvent,
) -> api::HostCommandListChangedEvent {
    let mut commands = snapshot
        .config
        .as_ref()
        .and_then(|config| config.host_commands.as_ref())
        .into_iter()
        .flat_map(|commands| commands.iter())
        .map(|(name, command)| {
            let parameters = command
                .parameters
                .iter()
                .flatten()
                .map(|(name, parameter)| {
                    (
                        name.clone(),
                        api::HostCommandParameter {
                            required: parameter.default.is_none()
                                && parameter.required.unwrap_or(true),
                            default: parameter.default.clone(),
                            allowed_values: parameter.allowed_values.clone(),
                            pattern: parameter.pattern.clone(),
                        },
                    )
                })
                .collect();
            let inputs = command
                .inputs
                .iter()
                .flatten()
                .map(|(name, input)| {
                    (
                        name.clone(),
                        api::HostCommandInput {
                            repository: input.repository.clone(),
                            capture: resolve_capture(input.capture),
                        },
                    )
                })
                .collect();
            let environment_names = command
                .environment
                .as_ref()
                .into_iter()
                .flat_map(|environment| {
                    environment.inherit.iter().flatten().cloned().chain(
                        environment
                            .values
                            .iter()
                            .flatten()
                            .map(|(name, _)| name.clone()),
                    )
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            api::HostCommand {
                name: name.clone(),
                description: command.description.clone(),
                program: command.program.clone(),
                arguments: command.arguments.clone().unwrap_or_default(),
                working_directory: command.working_directory.clone(),
                parameters,
                inputs,
                environment_names: environment_names.into(),
                timeout_seconds: command.timeout_seconds,
            }
        })
        .collect::<Vec<_>>();
    commands.sort_by(|left, right| left.name.cmp(&right.name));
    api::HostCommandListChangedEvent {
        value: api::HostCommandList {
            commands: commands.into(),
            configuration_error: snapshot.last_config_error.map(|error| error.message),
        },
    }
}
