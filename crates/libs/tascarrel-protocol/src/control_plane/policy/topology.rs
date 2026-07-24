//! Actor policies for directional links in the Tascarrel daemon topology.
//!
//! Each constructor describes the origins and callers accepted from the
//! authenticated peer. These policies intentionally have no access to an
//! operation's target, name, or input.

use tascarrel_api::types::pods::PodId;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::workspaces::WorkspaceName;

use super::ActorPolicy;

/// Accepts one authenticated client as both origin and caller.
///
/// Missing context is assigned to `client_id`. A supplied context must identify
/// that client as both origin and caller.
#[must_use]
pub fn client_to_hostd(client_id: &wire::ClientId) -> ActorPolicy {
    ActorPolicy::assigning(wire::Actor::Client(wire::ClientActor {
        client_id: client_id.clone(),
    }))
}

/// Accepts actors introduced by hostd on a guestd link.
///
/// Hostd may introduce itself and its authenticated client sessions. The
/// implementing guest service performs target and operation authorization.
#[must_use]
pub fn hostd_to_guestd() -> ActorPolicy {
    ActorPolicy::requiring(
        wire::Actor::is_host_or_client,
        wire::Actor::is_host_or_client,
    )
}

/// Accepts the authenticated guestd workspace and its pod actors on a hostd
/// link.
///
/// The implementing host service performs target and operation authorization.
#[must_use]
pub fn guestd_to_hostd(workspace: &WorkspaceName) -> ActorPolicy {
    let origin_workspace = workspace.clone();
    let caller_workspace = workspace.clone();
    ActorPolicy::requiring(
        move |candidate| workspace_or_pod(candidate, &origin_workspace),
        move |candidate| workspace_or_pod(candidate, &caller_workspace),
    )
}

/// Accepts actors guestd may carry or introduce on a podd link.
///
/// A guestd may carry host and client actors or introduce its own workspace
/// actor. The implementing pod service performs target and operation
/// authorization.
#[must_use]
pub fn guestd_to_podd(workspace: &WorkspaceName) -> ActorPolicy {
    let origin_workspace = workspace.clone();
    let caller_workspace = workspace.clone();
    ActorPolicy::requiring(
        move |actor| host_or_workspace(actor, &origin_workspace),
        move |actor| host_or_workspace(actor, &caller_workspace),
    )
}

/// Accepts the authenticated podd actor on its guestd link.
///
/// The implementing guest service performs target and operation authorization.
#[must_use]
pub fn podd_to_guestd(workspace: &WorkspaceName, pod_id: &PodId) -> ActorPolicy {
    let origin = wire::Actor::Pod(wire::PodAddress {
        workspace: workspace.clone(),
        pod_id: pod_id.clone(),
    });
    let caller = origin.clone();
    ActorPolicy::requiring(
        move |candidate| *candidate == origin,
        move |candidate| *candidate == caller,
    )
}

/// Assigns the authenticated pod actor to root operations opened by podctl.
///
/// The socket listener is provisioned for one pod by guestd. Podctl therefore
/// does not supply identity fields which could diverge from that boundary.
#[must_use]
pub fn podctl_to_guestd(workspace: &WorkspaceName, pod_id: &PodId) -> ActorPolicy {
    ActorPolicy::assigning(wire::Actor::Pod(wire::PodAddress {
        workspace: workspace.clone(),
        pod_id: pod_id.clone(),
    }))
}

/// Creates the actor authenticated for one guest daemon.
#[cfg(test)]
fn workspace_actor(workspace: &WorkspaceName) -> wire::Actor {
    wire::Actor::Workspace(wire::WorkspaceAddress {
        workspace: workspace.clone(),
    })
}

/// Tests whether an actor may be introduced by one guest daemon.
fn host_or_workspace(actor: &wire::Actor, workspace: &WorkspaceName) -> bool {
    actor.is_host_or_client()
        || matches!(
            actor,
            wire::Actor::Workspace(address) if address.workspace == *workspace
        )
}

fn workspace_or_pod(actor: &wire::Actor, workspace: &WorkspaceName) -> bool {
    matches!(
        actor,
        wire::Actor::Workspace(address) if address.workspace == *workspace
    ) || matches!(
        actor,
        wire::Actor::Pod(address) if address.workspace == *workspace
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::policy::Policy as _;
    use crate::control_plane::policy::PolicyResult;

    /// Verifies client and host identities accepted on downward links.
    #[test]
    fn downward_policies_accept_authenticated_actor_scopes() {
        let alpha = workspace("alpha");
        let beta = workspace("beta");
        let client_id = wire::ClientId::generate();
        let client_actor = wire::Actor::Client(wire::ClientActor {
            client_id: client_id.clone(),
        });
        let client_policy = client_to_hostd(&client_id);
        let host_policy = hostd_to_guestd();
        let guest_policy = guestd_to_podd(&alpha);
        let host_context = context(wire::Actor::Host, client_actor.clone());

        let assigned = client_policy
            .validate_context(None)
            .expect("client context is assigned");
        assert_eq!(assigned.origin, client_actor);
        assert_eq!(assigned.caller, client_actor);
        assert_eq!(
            host_policy
                .validate_context(Some(&host_context))
                .expect("host actors pass guest-link policy validation"),
            host_context
        );
        assert!(
            guest_policy
                .validate_context(Some(&context(
                    client_actor.clone(),
                    workspace_actor(&alpha),
                )))
                .is_ok()
        );
        assert_forbidden(
            &guest_policy.validate_context(Some(&context(client_actor, workspace_actor(&beta)))),
        );
    }

    /// Verifies guest and pod identities accepted on upward links.
    #[test]
    fn upward_policies_accept_only_the_authenticated_daemon() {
        let alpha = workspace("alpha");
        let beta = workspace("beta");
        let pod_id = PodId::generate();
        let guest_actor = workspace_actor(&alpha);
        let pod_actor = wire::Actor::Pod(wire::PodAddress {
            workspace: alpha.clone(),
            pod_id: pod_id.clone(),
        });
        let host_policy = guestd_to_hostd(&alpha);
        let guest_policy = podd_to_guestd(&alpha, &pod_id);

        assert!(
            host_policy
                .validate_context(Some(&context(guest_actor.clone(), guest_actor)))
                .is_ok()
        );
        assert!(
            host_policy
                .validate_context(Some(&context(pod_actor.clone(), pod_actor.clone())))
                .is_ok()
        );
        assert_forbidden(&host_policy.validate_context(Some(&context(
            workspace_actor(&beta),
            workspace_actor(&beta),
        ))));
        assert_forbidden(&host_policy.validate_context(Some(&context(
            wire::Actor::Pod(wire::PodAddress {
                workspace: beta.clone(),
                pod_id: pod_id.clone(),
            }),
            wire::Actor::Pod(wire::PodAddress {
                workspace: beta,
                pod_id: pod_id.clone(),
            }),
        ))));
        assert!(
            guest_policy
                .validate_context(Some(&context(pod_actor.clone(), pod_actor)))
                .is_ok()
        );
        assert!(matches!(
            guest_policy.validate_context(None),
            Err(error) if matches!(error.error(), wire::OperationError::InvalidRequest(_))
        ));
    }

    /// Verifies a pod-local client receives identity from its authenticated
    /// listener and cannot replace it with a supplied context.
    #[test]
    fn podctl_policy_assigns_the_listener_identity() {
        let alpha = workspace("alpha");
        let pod_id = PodId::generate();
        let policy = podctl_to_guestd(&alpha, &pod_id);
        let assigned = policy
            .validate_context(None)
            .expect("missing context receives the listener identity");
        let expected = wire::Actor::Pod(wire::PodAddress {
            workspace: alpha.clone(),
            pod_id: pod_id.clone(),
        });
        assert_eq!(assigned.origin, expected);
        assert_eq!(assigned.caller, expected);

        let other = wire::Actor::Pod(wire::PodAddress {
            workspace: alpha,
            pod_id: PodId::generate(),
        });
        assert_forbidden(&policy.validate_context(Some(&context(other.clone(), other))));
    }

    fn workspace(value: &str) -> WorkspaceName {
        WorkspaceName::new(value)
    }

    fn context(origin: wire::Actor, caller: wire::Actor) -> wire::RequestContext {
        wire::RequestContext {
            origin,
            caller,
            trace_id: wire::TraceId::generate(),
            caused_by: None,
        }
    }

    fn assert_forbidden(result: &PolicyResult) {
        assert!(matches!(
            result,
            Err(error) if matches!(error.error(), wire::OperationError::Forbidden(_))
        ));
    }
}
