import type { chats } from "../../../api/generated/index.ts";
import { formatCompactNumber, formatNumber } from "./usage.ts";

export interface ContextUsagePresentation {
  description: string;
  value: string;
}

export function presentContextUsage(
  usage: chats.ChatContextUsage | undefined,
): ContextUsagePresentation {
  if (!usage) {
    return {
      description: "Current context usage is unavailable",
      value: "N/A",
    };
  }

  const used = BigInt(String(usage.usedTokens));
  const contextWindow = usage.contextWindowTokens === undefined
    ? undefined
    : BigInt(String(usage.contextWindowTokens));
  const estimateMarker = usage.accuracy === "Estimated" ? "~" : "";
  const value = contextWindow === undefined
    ? `${estimateMarker}${formatCompactNumber(used)}`
    : `${estimateMarker}${formatCompactNumber(used)} / ${formatCompactNumber(contextWindow)}`;
  const accuracy = usage.accuracy === "Estimated" ? "Estimated current context" : "Current context";
  return {
    description: contextWindow === undefined
      ? `${accuracy}: ${formatNumber(used)} tokens`
      : `${accuracy}: ${formatNumber(used)} of ${formatNumber(contextWindow)} tokens`,
    value,
  };
}
