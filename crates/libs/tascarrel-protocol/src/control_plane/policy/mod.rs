//! Request-actor policies for control-plane operation openings.
//!
//! [`Policy`] validates only the origin and caller carried by a request
//! context. The [`topology`] module provides the standard policies for links
//! between clients and Tascarrel daemons. Operation targets, names, and inputs
//! are authorized by the service that implements them.

use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::protocol as wire;

pub mod topology;

/// Validates request actors received from one authenticated peer.
///
/// Returning a context admits the peer's origin and caller declarations and
/// attaches the validated or newly assigned context to the opening delivered
/// by the control driver. Returning a reported [`wire::OperationError`]
/// rejects the opening without exposing its operation payload to the policy.
pub trait Policy: Send + Sync + 'static {
    /// Validates or assigns trusted request context.
    ///
    /// # Errors
    ///
    /// Returns the peer-visible failure that rejects the context.
    fn validate_context(&self, context: Option<&wire::RequestContext>) -> PolicyResult;
}

impl<F> Policy for F
where
    F: Fn(Option<&wire::RequestContext>) -> PolicyResult + Send + Sync + 'static,
{
    fn validate_context(&self, context: Option<&wire::RequestContext>) -> PolicyResult {
        self(context)
    }
}

/// An immutable allowlist for request origins and callers.
pub struct ActorPolicy {
    missing_context: MissingContext,
    origin_rule: ActorRule,
    caller_rule: ActorRule,
}

impl ActorPolicy {
    /// Requires context whose origin and caller match their respective rules.
    pub(crate) fn requiring<Origin, Caller>(origin: Origin, caller: Caller) -> Self
    where
        Origin: Fn(&wire::Actor) -> bool + Send + Sync + 'static,
        Caller: Fn(&wire::Actor) -> bool + Send + Sync + 'static,
    {
        Self {
            missing_context: MissingContext::Reject,
            origin_rule: Box::new(origin),
            caller_rule: Box::new(caller),
        }
    }

    /// Assigns missing context to one authenticated actor and requires supplied
    /// contexts to identify the same actor as both origin and caller.
    pub(crate) fn assigning(actor: wire::Actor) -> Self {
        let origin = actor.clone();
        let caller = actor.clone();
        Self {
            missing_context: MissingContext::Assign(actor),
            origin_rule: Box::new(move |candidate| *candidate == origin),
            caller_rule: Box::new(move |candidate| *candidate == caller),
        }
    }
}

impl Policy for ActorPolicy {
    fn validate_context(&self, context: Option<&wire::RequestContext>) -> PolicyResult {
        match context {
            Some(context)
                if (self.origin_rule)(&context.origin) && (self.caller_rule)(&context.caller) =>
            {
                Ok(context.clone())
            }
            Some(_) => Err(context_actor_forbidden()),
            None => match &self.missing_context {
                MissingContext::Reject => Err(context_required()),
                MissingContext::Assign(actor) => Ok(wire::RequestContext {
                    origin: actor.clone(),
                    caller: actor.clone(),
                    trace_id: wire::TraceId::generate(),
                    caused_by: None,
                }),
            },
        }
    }
}

/// Result returned by a [`Policy`].
pub type PolicyResult = std::result::Result<wire::RequestContext, Report<wire::OperationError>>;

/// Policy that rejects every peer-originated operation opening.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAll;

impl Policy for DenyAll {
    fn validate_context(&self, _context: Option<&wire::RequestContext>) -> PolicyResult {
        Err(opening_forbidden())
    }
}

/// Determines how an actor policy handles an absent request context.
enum MissingContext {
    /// Rejects an opening without context.
    Reject,
    /// Assigns the authenticated actor to the opening.
    Assign(wire::Actor),
}

/// Predicate accepting actors in one request-context position.
type ActorRule = Box<dyn Fn(&wire::Actor) -> bool + Send + Sync>;

/// Creates a reported failure for a peer forbidden from opening operations.
fn opening_forbidden() -> Report<wire::OperationError> {
    wire::OperationError::Forbidden(wire::OperationErrorDetails {
        message: "authenticated peer is forbidden from opening operations on this link".into(),
        report: None,
    })
    .report()
}

/// Creates a reported failure for a missing request context.
fn context_required() -> Report<wire::OperationError> {
    wire::OperationError::InvalidRequest(wire::OperationErrorDetails {
        message: "request context is required on this link".into(),
        report: None,
    })
    .report()
}

/// Creates a reported failure for an actor outside the authenticated scope.
fn context_actor_forbidden() -> Report<wire::OperationError> {
    wire::OperationError::Forbidden(wire::OperationErrorDetails {
        message: "request origin or caller is not accepted from the authenticated peer".into(),
        report: None,
    })
    .report()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies independent origin and caller validation.
    #[test]
    fn actor_policy_validates_only_context_actors() {
        let workspace = wire::Actor::Workspace(wire::WorkspaceAddress {
            workspace: tascarrel_api::types::workspaces::WorkspaceName::new("alpha"),
        });
        let policy = ActorPolicy::requiring(
            |actor| matches!(actor, wire::Actor::Client(_) | wire::Actor::Host),
            {
                let workspace = workspace.clone();
                move |actor| *actor == workspace
            },
        );
        let accepted = wire::RequestContext {
            origin: wire::Actor::Host,
            caller: workspace,
            trace_id: wire::TraceId::generate(),
            caused_by: Some("causing-operation".into()),
        };
        let rejected = wire::RequestContext {
            caller: wire::Actor::Host,
            ..accepted.clone()
        };

        assert_eq!(
            policy
                .validate_context(Some(&accepted))
                .expect("accepted actors pass policy validation"),
            accepted
        );
        assert!(matches!(
            policy.validate_context(Some(&rejected)),
            Err(error) if matches!(error.error(), wire::OperationError::Forbidden(_))
        ));
        assert!(matches!(
            policy.validate_context(None),
            Err(error) if matches!(error.error(), wire::OperationError::InvalidRequest(_))
        ));
    }

    /// Verifies missing context assignment for authenticated client links.
    #[test]
    fn actor_policy_assigns_authenticated_actor() {
        let actor = wire::Actor::Client(wire::ClientActor {
            client_id: wire::ClientId::generate(),
        });
        let policy = ActorPolicy::assigning(actor.clone());

        let assigned = policy
            .validate_context(None)
            .expect("authenticated actor is assigned");

        assert_eq!(assigned.origin, actor);
        assert_eq!(assigned.caller, actor);
        assert!(assigned.caused_by.is_none());
    }
}
