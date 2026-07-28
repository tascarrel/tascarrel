//! Control-plane link lifecycle and subscription credit validation.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::protocol as wire;

use super::Error;
use super::Result;
use super::policy::Policy;
use super::render_operation_error;

/// Maximum terminal subscriptions retained to recognize crossed controls.
const FINISHED_SUBSCRIPTION_HISTORY_CAPACITY: usize = 1_024;

/// Active operation identifiers and subscription credit for one link.
#[derive(Default)]
pub(crate) struct LinkState {
    inbound_rpcs: HashSet<wire::InvocationId>,
    outbound_rpcs: HashSet<wire::InvocationId>,
    inbound_subscriptions: HashMap<wire::SubscriptionId, u64>,
    outbound_subscriptions: HashMap<wire::SubscriptionId, u64>,
    finished_inbound_subscriptions: HashSet<wire::SubscriptionId>,
    finished_inbound_subscription_order: VecDeque<wire::SubscriptionId>,
}

impl LinkState {
    /// Validates and records a message received from the peer.
    pub(crate) fn receive<P: Policy>(
        &mut self,
        mut message: wire::Message,
        policy: &P,
    ) -> Result<Received> {
        match &mut message {
            wire::Message::Control(_) => {}
            wire::Message::Rpc(rpc) => {
                if let Some(failure) = self.receive_rpc(rpc, policy)? {
                    return Ok(Received::Reject(failure));
                }
            }
            wire::Message::Subscription(subscription) => {
                match self.receive_subscription(subscription, policy)? {
                    SubscriptionAdmission::Deliver => {}
                    SubscriptionAdmission::Ignore => return Ok(Received::Ignore),
                    SubscriptionAdmission::Reject(failure) => {
                        return Ok(Received::Reject(*failure));
                    }
                }
            }
        }
        Ok(Received::Deliver(message))
    }

    /// Validates a local message without changing state before it is sent.
    pub(crate) fn prepare_send(&self, message: &wire::Message) -> Result<StateUpdate> {
        match message {
            wire::Message::Control(_) => Ok(StateUpdate::None),
            wire::Message::Rpc(rpc) => match rpc {
                wire::RpcMessage::Invoke(invocation) => {
                    if self.outbound_rpcs.contains(&invocation.id) {
                        Err(protocol_error("RPC invocation ID is already active"))
                    } else {
                        Ok(StateUpdate::AddOutboundRpc(invocation.id.clone()))
                    }
                }
                wire::RpcMessage::Cancel(cancellation) => {
                    require(
                        self.outbound_rpcs.contains(&cancellation.id),
                        "cannot cancel an unknown RPC invocation",
                    )?;
                    Ok(StateUpdate::None)
                }
                wire::RpcMessage::Canceled(canceled) => self.finish_inbound_rpc(&canceled.id),
                wire::RpcMessage::Completed(completed) => self.finish_inbound_rpc(&completed.id),
                wire::RpcMessage::Failed(failed) => self.finish_inbound_rpc(&failed.id),
            },
            wire::Message::Subscription(subscription) => match subscription {
                wire::SubscriptionMessage::Subscribe(start) => {
                    if self.outbound_subscriptions.contains_key(&start.id) {
                        Err(protocol_error("subscription ID is already active"))
                    } else {
                        Ok(StateUpdate::AddOutboundSubscription(start.id.clone()))
                    }
                }
                wire::SubscriptionMessage::GrantCredit(credit) => {
                    let available =
                        self.outbound_subscriptions.get(&credit.id).ok_or_else(|| {
                            protocol_error("cannot grant credit to an unknown subscription")
                        })?;
                    let updated = available
                        .checked_add(u64::from(credit.events))
                        .ok_or_else(|| protocol_error("subscription credit overflowed"))?;
                    Ok(StateUpdate::SetOutboundCredit(credit.id.clone(), updated))
                }
                wire::SubscriptionMessage::Event(event) => {
                    let available = self.inbound_subscriptions.get(&event.id).ok_or_else(|| {
                        protocol_error("cannot emit an event for an unknown subscription")
                    })?;
                    if *available == 0 {
                        return Err(protocol_error(
                            "cannot emit a subscription event without credit",
                        ));
                    }
                    Ok(StateUpdate::SetInboundCredit(
                        event.id.clone(),
                        available - 1,
                    ))
                }
                wire::SubscriptionMessage::Unsubscribe(stop) => {
                    require(
                        self.outbound_subscriptions.contains_key(&stop.id),
                        "cannot stop an unknown subscription",
                    )?;
                    Ok(StateUpdate::None)
                }
                wire::SubscriptionMessage::Completed(completed) => {
                    self.finish_inbound_subscription(&completed.id)
                }
                wire::SubscriptionMessage::Failed(failed) => {
                    self.finish_inbound_subscription(&failed.id)
                }
            },
        }
    }

    /// Applies a prepared transition after its message has been sent.
    pub(crate) fn commit(&mut self, update: StateUpdate) {
        match update {
            StateUpdate::None => {}
            StateUpdate::AddOutboundRpc(id) => {
                self.outbound_rpcs.insert(id);
            }
            StateUpdate::RemoveInboundRpc(id) => {
                self.inbound_rpcs.remove(&id);
            }
            StateUpdate::AddOutboundSubscription(id) => {
                self.outbound_subscriptions.insert(id, 0);
            }
            StateUpdate::RemoveInboundSubscription(id) => {
                self.inbound_subscriptions.remove(&id);
                self.remember_finished_inbound_subscription(id);
            }
            StateUpdate::SetInboundCredit(id, credit) => {
                self.inbound_subscriptions.insert(id, credit);
            }
            StateUpdate::SetOutboundCredit(id, credit) => {
                self.outbound_subscriptions.insert(id, credit);
            }
        }
    }

    /// Validates and records one RPC message received from the peer.
    fn receive_rpc<P: Policy>(
        &mut self,
        rpc: &mut wire::RpcMessage,
        policy: &P,
    ) -> Result<Option<wire::Message>> {
        match rpc {
            wire::RpcMessage::Invoke(invocation) => {
                if self.inbound_rpcs.contains(&invocation.id) {
                    return Err(protocol_error("peer reused an active RPC invocation ID"));
                }
                match policy.validate_context(invocation.context.as_ref()) {
                    Ok(context) => {
                        invocation.context = Some(context);
                        self.inbound_rpcs.insert(invocation.id.clone());
                    }
                    Err(error) => {
                        return Ok(Some(rpc_failure(
                            invocation.id.clone(),
                            render_operation_error(error),
                        )));
                    }
                }
            }
            wire::RpcMessage::Cancel(cancellation) => {
                require(
                    self.inbound_rpcs.contains(&cancellation.id),
                    "peer cancelled an unknown RPC invocation",
                )?;
            }
            wire::RpcMessage::Canceled(canceled) => {
                remove(
                    &mut self.outbound_rpcs,
                    &canceled.id,
                    "peer canceled an unknown RPC invocation",
                )?;
            }
            wire::RpcMessage::Completed(completed) => {
                remove(
                    &mut self.outbound_rpcs,
                    &completed.id,
                    "peer completed an unknown RPC invocation",
                )?;
            }
            wire::RpcMessage::Failed(failed) => {
                remove(
                    &mut self.outbound_rpcs,
                    &failed.id,
                    "peer failed an unknown RPC invocation",
                )?;
            }
        }
        Ok(None)
    }

    /// Validates and records one subscription message received from the peer.
    fn receive_subscription<P: Policy>(
        &mut self,
        subscription: &mut wire::SubscriptionMessage,
        policy: &P,
    ) -> Result<SubscriptionAdmission> {
        match subscription {
            wire::SubscriptionMessage::Subscribe(start) => {
                if self.inbound_subscriptions.contains_key(&start.id) {
                    return Err(protocol_error("peer reused an active subscription ID"));
                }
                match policy.validate_context(start.context.as_ref()) {
                    Ok(context) => {
                        self.forget_finished_inbound_subscription(&start.id);
                        start.context = Some(context);
                        self.inbound_subscriptions.insert(start.id.clone(), 0);
                    }
                    Err(error) => {
                        return Ok(SubscriptionAdmission::Reject(Box::new(
                            subscription_failure(start.id.clone(), render_operation_error(error)),
                        )));
                    }
                }
            }
            wire::SubscriptionMessage::GrantCredit(credit) => {
                let Some(available) = self.inbound_subscriptions.get_mut(&credit.id) else {
                    if self.finished_inbound_subscriptions.contains(&credit.id) {
                        return Ok(SubscriptionAdmission::Ignore);
                    }
                    return Err(protocol_error(
                        "peer granted credit to an unknown subscription",
                    ));
                };
                *available = available
                    .checked_add(u64::from(credit.events))
                    .ok_or_else(|| protocol_error("subscription credit overflowed"))?;
            }
            wire::SubscriptionMessage::Event(event) => {
                consume_credit(
                    &mut self.outbound_subscriptions,
                    &event.id,
                    "peer emitted an event for an unknown subscription",
                    "peer emitted an event without credit",
                )?;
            }
            wire::SubscriptionMessage::Unsubscribe(stop) => {
                if !self.inbound_subscriptions.contains_key(&stop.id) {
                    if self.finished_inbound_subscriptions.contains(&stop.id) {
                        return Ok(SubscriptionAdmission::Ignore);
                    }
                    return Err(protocol_error("peer stopped an unknown subscription"));
                }
            }
            wire::SubscriptionMessage::Completed(completed) => {
                remove_map(
                    &mut self.outbound_subscriptions,
                    &completed.id,
                    "peer completed an unknown subscription",
                )?;
            }
            wire::SubscriptionMessage::Failed(failed) => {
                remove_map(
                    &mut self.outbound_subscriptions,
                    &failed.id,
                    "peer failed an unknown subscription",
                )?;
            }
        }
        Ok(SubscriptionAdmission::Deliver)
    }

    /// Prepares removal of an RPC opened by the peer.
    fn finish_inbound_rpc(&self, id: &wire::InvocationId) -> Result<StateUpdate> {
        require(
            self.inbound_rpcs.contains(id),
            "cannot finish an unknown RPC invocation",
        )?;
        Ok(StateUpdate::RemoveInboundRpc(id.clone()))
    }

    /// Prepares removal of a subscription opened by the peer.
    fn finish_inbound_subscription(&self, id: &wire::SubscriptionId) -> Result<StateUpdate> {
        require(
            self.inbound_subscriptions.contains_key(id),
            "cannot finish an unknown subscription",
        )?;
        Ok(StateUpdate::RemoveInboundSubscription(id.clone()))
    }

    /// Remembers terminal subscriptions long enough to tolerate crossed peer
    /// controls.
    fn remember_finished_inbound_subscription(&mut self, id: wire::SubscriptionId) {
        let inserted = self.finished_inbound_subscriptions.insert(id.clone());
        debug_assert!(inserted, "active subscription IDs are unique");
        self.finished_inbound_subscription_order.push_back(id);
        while self.finished_inbound_subscription_order.len()
            > FINISHED_SUBSCRIPTION_HISTORY_CAPACITY
        {
            let expired = self
                .finished_inbound_subscription_order
                .pop_front()
                .expect("finished subscription history is not empty");
            self.finished_inbound_subscriptions.remove(&expired);
        }
    }

    /// Forgets a terminal identifier when the peer explicitly reuses it.
    fn forget_finished_inbound_subscription(&mut self, id: &wire::SubscriptionId) {
        if self.finished_inbound_subscriptions.remove(id) {
            self.finished_inbound_subscription_order
                .retain(|finished| finished != id);
        }
    }
}

/// Result of validating a message received from the peer.
pub(crate) enum Received {
    /// The admitted message should be delivered to the application.
    Deliver(wire::Message),
    /// The generated failure should be returned directly to the peer.
    Reject(wire::Message),
    /// A harmless control crossed the subscription's terminal message.
    Ignore,
}

/// Admission result for one peer subscription message.
enum SubscriptionAdmission {
    /// The message should be delivered to the application.
    Deliver,
    /// The message should be answered with a generated failure.
    Reject(Box<wire::Message>),
    /// The message is a harmless late control for a terminal subscription.
    Ignore,
}

/// State transition committed after a local message is sent.
pub(crate) enum StateUpdate {
    /// The message leaves lifecycle state unchanged.
    None,
    /// Records an RPC opened by the local application.
    AddOutboundRpc(wire::InvocationId),
    /// Removes an RPC opened by the peer.
    RemoveInboundRpc(wire::InvocationId),
    /// Records a subscription opened by the local application.
    AddOutboundSubscription(wire::SubscriptionId),
    /// Removes a subscription opened by the peer.
    RemoveInboundSubscription(wire::SubscriptionId),
    /// Updates credit for a subscription opened by the peer.
    SetInboundCredit(wire::SubscriptionId, u64),
    /// Updates credit for a subscription opened by the local application.
    SetOutboundCredit(wire::SubscriptionId, u64),
}

/// Creates an RPC failure for a rejected invocation.
fn rpc_failure(id: wire::InvocationId, error: wire::OperationError) -> wire::Message {
    wire::Message::Rpc(wire::RpcMessage::Failed(wire::RpcFailed { id, error }))
}

/// Creates a subscription failure for a rejected opening.
fn subscription_failure(id: wire::SubscriptionId, error: wire::OperationError) -> wire::Message {
    wire::Message::Subscription(wire::SubscriptionMessage::Failed(
        wire::SubscriptionFailed { id, error },
    ))
}

/// Removes one active identifier or reports a peer protocol violation.
fn remove<T>(set: &mut HashSet<T>, value: &T, message: &str) -> Result<()>
where
    T: Eq + std::hash::Hash,
{
    require(set.remove(value), message)
}

/// Removes one active keyed entry or reports a peer protocol violation.
fn remove_map<K, V>(map: &mut HashMap<K, V>, key: &K, message: &str) -> Result<()>
where
    K: Eq + std::hash::Hash,
{
    require(map.remove(key).is_some(), message)
}

/// Consumes one event credit from an active subscription.
fn consume_credit(
    subscriptions: &mut HashMap<wire::SubscriptionId, u64>,
    id: &wire::SubscriptionId,
    unknown: &str,
    exhausted: &str,
) -> Result<()> {
    let available = subscriptions
        .get_mut(id)
        .ok_or_else(|| protocol_error(unknown))?;
    if *available == 0 {
        return Err(protocol_error(exhausted));
    }
    *available -= 1;
    Ok(())
}

/// Reports a protocol violation when `condition` is false.
fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(protocol_error(message))
    }
}

/// Wraps a protocol violation in an error report.
fn protocol_error(message: &str) -> Report<Error> {
    Error::Protocol(message.to_owned()).report()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::policy::DenyAll;

    /// Verifies crossed controls for a terminal subscription do not close the
    /// link.
    #[test]
    fn late_terminal_subscription_controls_are_ignored() {
        let mut state = LinkState::default();
        let id = wire::SubscriptionId::generate();
        state.inbound_subscriptions.insert(id.clone(), 0);
        let completed = wire::Message::Subscription(wire::SubscriptionMessage::Completed(
            wire::SubscriptionCompleted { id: id.clone() },
        ));
        let update = state.prepare_send(&completed).expect("prepare completion");
        state.commit(update);

        let late_credit = wire::Message::Subscription(wire::SubscriptionMessage::GrantCredit(
            wire::SubscriptionCredit {
                id: id.clone(),
                events: 1,
            },
        ));
        assert!(matches!(
            state.receive(late_credit, &DenyAll).unwrap(),
            Received::Ignore
        ));

        let late_stop = wire::Message::Subscription(wire::SubscriptionMessage::Unsubscribe(
            wire::SubscriptionStop { id },
        ));
        assert!(matches!(
            state.receive(late_stop, &DenyAll).unwrap(),
            Received::Ignore
        ));

        let unknown_credit = wire::Message::Subscription(wire::SubscriptionMessage::GrantCredit(
            wire::SubscriptionCredit {
                id: wire::SubscriptionId::generate(),
                events: 1,
            },
        ));
        let Err(error) = state.receive(unknown_credit, &DenyAll) else {
            panic!("unknown credit remains invalid");
        };
        assert!(matches!(error.error(), Error::Protocol(_)));
    }
}
