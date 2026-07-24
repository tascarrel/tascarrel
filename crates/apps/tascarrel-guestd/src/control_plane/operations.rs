//! Shared typed-operation traits and wire conversion helpers.

use async_trait::async_trait;
use reportify::Report;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tascarrel_api::GuestAction;
use tascarrel_api::GuestSubscription;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::store as store_api;

use super::InvocationCtx;
use super::SubscriptionCtx;

/// Executes one decoded action against the composed guest state.
#[async_trait]
pub(crate) trait ExecuteAction: GuestAction {
    /// Checks caller permissions with the decoded action and complete request.
    fn check_permissions(
        &self,
        context: &InvocationCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>>;

    /// Executes the action after its permission check succeeds.
    async fn execute(
        self,
        context: InvocationCtx<'_>,
    ) -> Result<Self::Output, Report<wire::OperationError>>;
}

/// Opens one decoded subscription against the composed guest state.
#[async_trait]
pub(crate) trait OpenSubscription: GuestSubscription {
    /// Checks caller permissions with the decoded subscription and complete
    /// request.
    fn check_permissions(
        &self,
        context: &SubscriptionCtx<'_>,
    ) -> Result<(), Report<wire::OperationError>>;

    /// Typed event source returned by the feature service.
    type Source: EventSource<Event = Self::Event>;

    /// Opens the subscription after its permission check succeeds.
    async fn open(
        self,
        context: SubscriptionCtx<'_>,
    ) -> Result<Self::Source, Report<wire::OperationError>>;
}

/// Receives typed events from one feature subscription.
#[async_trait]
pub(crate) trait EventSource: Send + 'static {
    /// Event exposed by the subscription API.
    type Event: Serialize + DeserializeOwned + Send;

    /// Receives the next event, or `None` after completion.
    ///
    /// An operation failure terminates the subscription.
    async fn recv(&mut self) -> Result<Option<Self::Event>, Report<wire::OperationError>>;
}

/// Converts an in-memory store event to its API representation.
pub(crate) fn store_event<State, Mutation>(
    event: tascarrel_store::StoreEvent<State, Mutation>,
) -> store_api::StoreEvent<State, Mutation>
where
    State: Clone,
    Mutation: Clone,
{
    match event {
        tascarrel_store::StoreEvent::Snapshot(snapshot) => {
            store_api::StoreEvent::Snapshot(store_api::Snapshot {
                stamp: store_stamp(snapshot.stamp),
                value: (*snapshot.value).clone(),
            })
        }
        tascarrel_store::StoreEvent::Mutation(mutation) => {
            store_api::StoreEvent::Mutation(store_api::StampedMutation {
                stamp: store_stamp(mutation.stamp),
                mutation: (*mutation.mutation).clone(),
            })
        }
    }
}

/// Converts an in-memory store stamp to its API representation.
fn store_stamp(stamp: tascarrel_store::Stamp) -> store_api::Stamp {
    store_api::Stamp {
        generation: stamp.generation.to_string().into(),
        version: stamp.version,
    }
}
