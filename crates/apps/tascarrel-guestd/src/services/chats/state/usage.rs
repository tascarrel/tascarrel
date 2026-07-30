//! Workspace chat-usage aggregation by durable cost-center assignment.
//!
//! [`build_report`] combines the latest absolute usage snapshot for each turn.
//! Cache and reasoning counters retain their subset semantics, while cost
//! coverage remains explicit through the priced-turn count.

use std::collections::BTreeMap;
use std::collections::HashSet;

use jiff::Timestamp;
use reportify::Report;
use reportify::ResultExt as _;
use reportify::Whatever as _;
use tascarrel_api::ids::ChatId;
use tascarrel_api::types::chats::ChatCacheWriteUsage;
use tascarrel_api::types::chats::ChatCostCenterId;
use tascarrel_api::types::chats::ChatCostCenterUsage;
use tascarrel_api::types::chats::ChatTokenUsage;
use tascarrel_api::types::chats::ChatTurnUsage;
use tascarrel_api::types::chats::ChatUsageAggregate;
use tascarrel_api::types::chats::ChatUsageCoverage;
use tascarrel_api::types::chats::ChatUsageReport;
use tascarrel_api::types::chats::ChatUsageState;
use tascarrel_api::types::common::Money;
use tokio::sync::watch;

use crate::services::chats::state::storage::Storage;
use crate::services::chats::state::storage::StorageError;

reportify::new_whatever_type! {
    /// Failure while aggregating workspace chat usage.
    pub UsageAggregationError
}

/// One durable turn-usage snapshot together with its attribution dimensions.
pub struct UsageRecord {
    /// Chat containing the turn.
    pub chat_id: ChatId,
    /// Current cost-center assignment of the chat.
    pub cost_center_id: Option<ChatCostCenterId>,
    /// Latest durable usage snapshot for the turn.
    pub usage: ChatTurnUsage,
}

/// Latest-value subscription to a workspace chat-usage report.
pub struct UsageReportSubscription {
    storage: Storage,
    from: Timestamp,
    until: Timestamp,
    changes: watch::Receiver<u64>,
    initial_pending: bool,
}

impl UsageReportSubscription {
    /// Creates a subscription backed by the durable chat store.
    pub fn new(
        storage: Storage,
        from: Timestamp,
        until: Timestamp,
        changes: watch::Receiver<u64>,
    ) -> Self {
        Self {
            storage,
            from,
            until,
            changes,
            initial_pending: true,
        }
    }

    /// Receives the current report and then each changed replacement.
    pub async fn recv(&mut self) -> Result<Option<ChatUsageReport>, Report<StorageError>> {
        if self.initial_pending {
            self.initial_pending = false;
        } else if self.changes.changed().await.is_err() {
            return Ok(None);
        }
        self.storage
            .usage_report(self.from, self.until)
            .await
            .map(Some)
    }
}

/// Builds one complete report from records already restricted to its interval.
pub fn build_report(
    from: Timestamp,
    until: Timestamp,
    records: Vec<UsageRecord>,
) -> Result<ChatUsageReport, Report<UsageAggregationError>> {
    let mut total = AggregateBuilder::default();
    let mut cost_centers = BTreeMap::<Option<ChatCostCenterId>, AggregateBuilder>::new();
    for record in records {
        total.add(&record)?;
        cost_centers
            .entry(record.cost_center_id.clone())
            .or_default()
            .add(&record)?;
    }
    let cost_centers = cost_centers
        .into_iter()
        .map(|(cost_center_id, usage)| {
            Ok(ChatCostCenterUsage {
                cost_center_id,
                usage: usage.finish()?,
            })
        })
        .collect::<Result<Vec<_>, Report<UsageAggregationError>>>()?;
    Ok(ChatUsageReport {
        from,
        until,
        total: total.finish()?,
        cost_centers: cost_centers.into(),
    })
}

#[derive(Default)]
struct AggregateBuilder {
    chat_ids: HashSet<ChatId>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: OptionalCounter,
    cache_write_input_tokens: OptionalCounter,
    cache_writes_by_ttl: BTreeMap<u64, u64>,
    reasoning_output_tokens: OptionalCounter,
    calculated_costs: BTreeMap<String, u64>,
    turn_count: u64,
    priced_turn_count: u64,
    provisional_turn_count: u64,
    primary_agent_turn_count: u64,
}

impl AggregateBuilder {
    fn add(&mut self, record: &UsageRecord) -> Result<(), Report<UsageAggregationError>> {
        let tokens = &record.usage.snapshot.tokens;
        self.chat_ids.insert(record.chat_id.clone());
        self.input_tokens = checked_add(self.input_tokens, tokens.input_tokens)?;
        self.output_tokens = checked_add(self.output_tokens, tokens.output_tokens)?;
        self.cache_read_input_tokens
            .add(tokens.cache_read_input_tokens)?;
        self.cache_write_input_tokens
            .add(tokens.cache_write_input_tokens)?;
        self.reasoning_output_tokens
            .add(tokens.reasoning_output_tokens)?;
        for cache_write in &tokens.cache_writes_by_ttl {
            let current = self
                .cache_writes_by_ttl
                .get(&cache_write.ttl_seconds)
                .copied()
                .unwrap_or_default();
            self.cache_writes_by_ttl.insert(
                cache_write.ttl_seconds,
                checked_add(current, cache_write.input_tokens)?,
            );
        }
        self.turn_count = checked_add(self.turn_count, 1)?;
        if record.usage.state == ChatUsageState::Provisional {
            self.provisional_turn_count = checked_add(self.provisional_turn_count, 1)?;
        }
        if record.usage.snapshot.coverage == ChatUsageCoverage::PrimaryAgent {
            self.primary_agent_turn_count = checked_add(self.primary_agent_turn_count, 1)?;
        }
        if let Some(cost) = &record.usage.calculated_cost {
            self.priced_turn_count = checked_add(self.priced_turn_count, 1)?;
            let currency = cost.amount.currency.to_string();
            let current = self
                .calculated_costs
                .get(&currency)
                .copied()
                .unwrap_or_default();
            self.calculated_costs
                .insert(currency, checked_add(current, cost.amount.amount)?);
        }
        Ok(())
    }

    fn finish(self) -> Result<ChatUsageAggregate, Report<UsageAggregationError>> {
        let chat_count = u64::try_from(self.chat_ids.len())
            .whatever("chat count does not fit in an unsigned 64-bit integer")?;
        let cache_writes_by_ttl = self
            .cache_writes_by_ttl
            .into_iter()
            .map(|(ttl_seconds, input_tokens)| ChatCacheWriteUsage {
                ttl_seconds,
                input_tokens,
            })
            .collect::<Vec<_>>()
            .into();
        let calculated_costs = self
            .calculated_costs
            .into_iter()
            .map(|(currency, amount)| Money {
                currency: currency.into(),
                amount,
            })
            .collect::<Vec<_>>()
            .into();
        Ok(ChatUsageAggregate {
            tokens: ChatTokenUsage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cache_read_input_tokens: self.cache_read_input_tokens.value(),
                cache_write_input_tokens: self.cache_write_input_tokens.value(),
                cache_writes_by_ttl,
                reasoning_output_tokens: self.reasoning_output_tokens.value(),
            },
            calculated_costs,
            chat_count,
            turn_count: self.turn_count,
            priced_turn_count: self.priced_turn_count,
            provisional_turn_count: self.provisional_turn_count,
            primary_agent_turn_count: self.primary_agent_turn_count,
        })
    }
}

struct OptionalCounter {
    value: Option<u64>,
}

impl OptionalCounter {
    fn add(&mut self, value: Option<u64>) -> Result<(), Report<UsageAggregationError>> {
        self.value = self
            .value
            .zip(value)
            .map(|(current, value)| checked_add(current, value))
            .transpose()?;
        Ok(())
    }

    const fn value(self) -> Option<u64> {
        self.value
    }
}

impl Default for OptionalCounter {
    fn default() -> Self {
        Self { value: Some(0) }
    }
}

fn checked_add(left: u64, right: u64) -> Result<u64, Report<UsageAggregationError>> {
    left.checked_add(right).ok_or_else(|| {
        Report::new(UsageAggregationError::new()).message("chat usage aggregation overflowed")
    })
}

#[cfg(test)]
mod tests {
    use tascarrel_api::ArcVec;
    use tascarrel_api::types::chats::ChatCalculatedCost;
    use tascarrel_api::types::chats::ChatUsageSnapshot;

    use super::*;

    /// Confirms that reports partition usage by current assignment while
    /// retaining completeness and pricing metadata.
    #[test]
    fn aggregates_cost_centers_and_unknown_optional_counters() {
        let from = timestamp("2026-07-01T00:00:00Z");
        let until = timestamp("2026-08-01T00:00:00Z");
        let report = build_report(
            from,
            until,
            vec![
                UsageRecord {
                    chat_id: ChatId::generate(),
                    cost_center_id: None,
                    usage: usage(
                        ChatUsageState::Settled,
                        ChatUsageCoverage::ExecutionTree,
                        (100, 20, Some(40), Some(5)),
                        Some(3),
                    ),
                },
                UsageRecord {
                    chat_id: ChatId::generate(),
                    cost_center_id: Some(ChatCostCenterId::new("client_alpha")),
                    usage: usage(
                        ChatUsageState::Provisional,
                        ChatUsageCoverage::PrimaryAgent,
                        (200, 30, None, None),
                        Some(5),
                    ),
                },
            ],
        )
        .unwrap();

        assert_eq!(report.total.tokens.input_tokens, 300);
        assert_eq!(report.total.tokens.output_tokens, 50);
        assert_eq!(report.total.tokens.cache_read_input_tokens, None);
        assert_eq!(report.total.tokens.reasoning_output_tokens, None);
        assert_eq!(report.total.chat_count, 2);
        assert_eq!(report.total.turn_count, 2);
        assert_eq!(report.total.priced_turn_count, 2);
        assert_eq!(report.total.provisional_turn_count, 1);
        assert_eq!(report.total.primary_agent_turn_count, 1);
        assert_eq!(report.total.calculated_costs[0].amount, 8);
        assert_eq!(report.cost_centers.len(), 2);
        assert_eq!(report.cost_centers[0].cost_center_id, None);
        assert_eq!(
            report.cost_centers[1]
                .cost_center_id
                .as_ref()
                .map(ChatCostCenterId::as_str),
            Some("client_alpha")
        );
    }

    fn usage(
        state: ChatUsageState,
        coverage: ChatUsageCoverage,
        tokens: (u64, u64, Option<u64>, Option<u64>),
        cost: Option<u64>,
    ) -> ChatTurnUsage {
        ChatTurnUsage {
            state,
            observed_at: timestamp("2026-07-15T12:00:00Z"),
            snapshot: ChatUsageSnapshot {
                coverage,
                tokens: ChatTokenUsage {
                    input_tokens: tokens.0,
                    output_tokens: tokens.1,
                    cache_read_input_tokens: tokens.2,
                    cache_write_input_tokens: Some(0),
                    cache_writes_by_ttl: ArcVec::new(),
                    reasoning_output_tokens: tokens.3,
                },
                models: ArcVec::new(),
                provider_estimated_cost: None,
            },
            calculated_cost: cost.map(|amount| ChatCalculatedCost {
                amount: Money {
                    currency: "USD".into(),
                    amount,
                },
                pricing_catalog_version: "test".into(),
            }),
        }
    }

    fn timestamp(value: &str) -> Timestamp {
        value.parse().unwrap()
    }
}
