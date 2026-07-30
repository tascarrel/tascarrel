import type { chats } from "../../../api/generated/index.ts";

export interface UsagePresentation {
  breakdown: string;
  cost?: string;
  description: string;
  input: string;
  output: string;
  reasoning?: string;
  total: string;
}

export function presentTurnUsage(turn: chats.ChatTurn): UsagePresentation | undefined {
  return presentUsage([turn]);
}

export function presentChatUsage(turns: chats.ChatTurn[]): UsagePresentation | undefined {
  return presentUsage(turns);
}

function presentUsage(turns: chats.ChatTurn[]): UsagePresentation | undefined {
  let input = 0n;
  let output = 0n;
  let cached = 0n;
  let reasoning = 0n;
  let cachedKnown = true;
  let reasoningKnown = true;
  let usageCount = 0;
  let pricedUsageCount = 0;
  const costs = new Map<string, bigint>();

  for (const turn of turns) {
    const usage = turn.usage;
    if (!usage) continue;

    usageCount += 1;
    const tokens = usage.snapshot.tokens;
    input += BigInt(String(tokens.inputTokens));
    output += BigInt(String(tokens.outputTokens));
    if (tokens.cacheReadInputTokens === undefined) {
      cachedKnown = false;
    } else {
      cached += BigInt(String(tokens.cacheReadInputTokens));
    }
    if (tokens.reasoningOutputTokens === undefined) {
      reasoningKnown = false;
    } else {
      reasoning += BigInt(String(tokens.reasoningOutputTokens));
    }

    const cost = usage.calculatedCost?.amount;
    if (cost) {
      pricedUsageCount += 1;
      costs.set(
        cost.currency,
        (costs.get(cost.currency) ?? 0n) + BigInt(String(cost.amount)),
      );
    }
  }

  if (usageCount === 0) return undefined;

  const uncachedInput = cachedKnown ? input - cached : input;
  const regularOutput = reasoningKnown ? output - reasoning : output;
  const formattedCosts =
    pricedUsageCount === usageCount
      ? [...costs].map(([currency, amount]) => formatMoney(currency, amount))
      : [];
  const breakdown = [
    cachedKnown
      ? `${formatCompactNumber(uncachedInput)} input (+${formatCompactNumber(cached)} cached)`
      : `${formatCompactNumber(input)} input`,
    `${formatCompactNumber(regularOutput)} output`,
    reasoningKnown ? `${formatCompactNumber(reasoning)} reasoning` : undefined,
    ...formattedCosts,
  ].filter((part): part is string => part !== undefined);
  const description = [
    `${formatNumber(uncachedInput)} uncached input`,
    cachedKnown ? `${formatNumber(cached)} cached input` : undefined,
    `${formatNumber(regularOutput)} output`,
    reasoningKnown ? `${formatNumber(reasoning)} reasoning output` : undefined,
  ].filter((part): part is string => part !== undefined);
  const total = input + output;
  const costDescription = formattedCosts.length === 0 ? "" : ` · ${formattedCosts.join(" + ")}`;

  return {
    breakdown: breakdown.join(", "),
    ...(formattedCosts.length > 0 ? { cost: formattedCosts.join(" + ") } : {}),
    description: `${formatNumber(total)} tokens (${description.join(", ")})${costDescription}`,
    input: formatCompactNumber(uncachedInput),
    output: formatCompactNumber(regularOutput),
    ...(reasoningKnown ? { reasoning: formatCompactNumber(reasoning) } : {}),
    total: formatCompactNumber(total),
  };
}

export function formatMoney(currency: string, amount: bigint): string {
  try {
    const formatter = new Intl.NumberFormat(undefined, {
      style: "currency",
      currency,
    });
    const minorUnitDigits = formatter.resolvedOptions().maximumFractionDigits ?? 2;
    return formatter.format(Number(amount) / 10 ** minorUnitDigits);
  } catch {
    return `${amount} ${currency}`;
  }
}

export function formatCompactNumber(value: bigint): string {
  return new Intl.NumberFormat(undefined, {
    notation: "compact",
    maximumFractionDigits: 1,
  }).format(value);
}

export function formatNumber(value: bigint): string {
  return new Intl.NumberFormat().format(value);
}
