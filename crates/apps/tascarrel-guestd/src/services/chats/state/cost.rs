//! Exact turn-cost calculation from versioned model pricing snapshots.

use tascarrel_api::types::chats::ChatCalculatedCost;
use tascarrel_api::types::chats::ChatModelPricing;
use tascarrel_api::types::chats::ChatTokenUsage;
use tascarrel_api::types::chats::ChatUsageSnapshot;
use tascarrel_api::types::common::Money;

/// Calculates an all-or-nothing cost for a normalized usage snapshot.
///
/// All model usages must carry pricing from the same catalog version, currency,
/// and token unit. Input, cache-read input, cache-write input, and output are
/// accumulated before the result is rounded to the nearest currency minor unit.
pub fn calculate_cost(usage: &ChatUsageSnapshot) -> Option<ChatCalculatedCost> {
    let first = usage.models.first()?.pricing.as_ref()?;
    let token_count = first.token_count;
    if token_count == 0 {
        return None;
    }

    let catalog_version = first.catalog_version.clone();
    let currency = first.input.currency.clone();
    let mut numerator = 0_u128;
    for model in &usage.models {
        let pricing = model.pricing.as_ref()?;
        if pricing.catalog_version != catalog_version || pricing.token_count != token_count {
            return None;
        }
        numerator =
            numerator.checked_add(model_cost_numerator(&model.tokens, pricing, &currency)?)?;
    }

    let denominator = u128::from(token_count);
    let amount = numerator.checked_add(denominator / 2)? / denominator;
    Some(ChatCalculatedCost {
        amount: Money {
            currency,
            amount: u64::try_from(amount).ok()?,
        },
        pricing_catalog_version: catalog_version,
    })
}

fn model_cost_numerator(
    usage: &ChatTokenUsage,
    pricing: &ChatModelPricing,
    currency: &str,
) -> Option<u128> {
    let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
    let cache_write = usage.cache_write_input_tokens.map_or_else(
        || {
            usage
                .cache_writes_by_ttl
                .iter()
                .try_fold(0_u64, |total, usage| total.checked_add(usage.input_tokens))
        },
        Some,
    )?;
    let classified_input = cache_read.checked_add(cache_write)?;
    let input = usage.input_tokens.checked_sub(classified_input)?;

    let mut total = 0_u128;
    total = add_cost(total, input, &pricing.input, currency)?;
    total = add_cost(
        total,
        cache_read,
        pricing.cache_read_input.as_ref().unwrap_or(&pricing.input),
        currency,
    )?;
    total = add_cost(
        total,
        cache_write,
        pricing.cache_write_input.as_ref().unwrap_or(&pricing.input),
        currency,
    )?;
    add_cost(total, usage.output_tokens, &pricing.output, currency)
}

fn add_cost(total: u128, tokens: u64, rate: &Money, currency: &str) -> Option<u128> {
    if rate.currency.as_ref() != currency {
        return None;
    }
    total.checked_add(u128::from(tokens).checked_mul(u128::from(rate.amount))?)
}

#[cfg(test)]
mod tests {
    use tascarrel_api::ArcVec;
    use tascarrel_api::types::chats::ChatModelSelection;
    use tascarrel_api::types::chats::ChatModelUsage;
    use tascarrel_api::types::chats::ChatUsageCoverage;

    use super::*;

    /// Confirms that every priced token category contributes to the calculated
    /// cost.
    #[test]
    fn calculates_distinct_input_categories_and_inclusive_output() {
        let pricing = pricing(2_500, 250, 3_125, 15_000);
        let usage = ChatUsageSnapshot {
            coverage: ChatUsageCoverage::ExecutionTree,
            tokens: tokens(10_000_000, 1_000_000, 2_000_000, 1_000_000),
            models: vec![ChatModelUsage {
                model: selection("gpt-5.6-terra"),
                tokens: tokens(10_000_000, 1_000_000, 2_000_000, 1_000_000),
                pricing: Some(pricing),
                provider_estimated_cost: None,
            }]
            .into(),
            provider_estimated_cost: None,
        };

        let cost = calculate_cost(&usage).unwrap();
        assert_eq!(cost.amount.currency.as_ref(), "USD");
        assert_eq!(cost.amount.amount, 3_613);
    }

    /// Confirms that fractional per-model costs are accumulated before
    /// minor-unit rounding.
    #[test]
    fn rounds_once_after_accumulating_all_models() {
        let pricing = ChatModelPricing {
            catalog_version: "test".into(),
            token_count: 10,
            input: money(1),
            cache_read_input: None,
            cache_write_input: None,
            output: money(0),
        };
        let model = |name| ChatModelUsage {
            model: selection(name),
            tokens: tokens(4, 0, 0, 0),
            pricing: Some(pricing.clone()),
            provider_estimated_cost: None,
        };
        let usage = ChatUsageSnapshot {
            coverage: ChatUsageCoverage::ExecutionTree,
            tokens: tokens(8, 0, 0, 0),
            models: vec![model("one"), model("two")].into(),
            provider_estimated_cost: None,
        };

        assert_eq!(calculate_cost(&usage).unwrap().amount.amount, 1);
    }

    fn pricing(
        input: u64,
        cache_read_input: u64,
        cache_write_input: u64,
        output: u64,
    ) -> ChatModelPricing {
        ChatModelPricing {
            catalog_version: "test".into(),
            token_count: 10_000_000,
            input: money(input),
            cache_read_input: Some(money(cache_read_input)),
            cache_write_input: Some(money(cache_write_input)),
            output: money(output),
        }
    }

    fn money(amount: u64) -> Money {
        Money {
            currency: "USD".into(),
            amount,
        }
    }

    fn selection(model: &str) -> ChatModelSelection {
        ChatModelSelection {
            model: model.into(),
            options: ArcVec::new(),
        }
    }

    fn tokens(input: u64, output: u64, cache_read: u64, cache_write: u64) -> ChatTokenUsage {
        ChatTokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: Some(cache_read),
            cache_write_input_tokens: Some(cache_write),
            cache_writes_by_ttl: ArcVec::new(),
            reasoning_output_tokens: Some(output),
        }
    }
}
