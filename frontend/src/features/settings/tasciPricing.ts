import type { config } from "../../api/generated/index.ts";

type TasciModelPricing = NonNullable<config.WorkspaceTasciModel["pricing"]>;

export type TasciPricingDraft = {
  enabled: boolean;
  catalogVersion: string;
  tokenCount: string;
  currency: string;
  inputAmount: string;
  cacheReadInputAmount: string;
  cacheWriteInputAmount: string;
  outputAmount: string;
};

/** Creates editable pricing state while preserving every configured rate. */
export function tasciPricingDraft(
  pricing?: TasciModelPricing,
): TasciPricingDraft {
  return {
    enabled: pricing !== undefined,
    catalogVersion: pricing?.catalogVersion ?? "workspace-settings:1",
    tokenCount: pricing?.tokenCount.toString() ?? "1000000",
    currency: pricing?.input.currency ?? "USD",
    inputAmount: pricing?.input.amount.toString() ?? "",
    cacheReadInputAmount: pricing?.cacheReadInput?.amount.toString() ?? "",
    cacheWriteInputAmount: pricing?.cacheWriteInput?.amount.toString() ?? "",
    outputAmount: pricing?.output.amount.toString() ?? "",
  };
}

/** Converts validated editable pricing state into the settings contract. */
export function tasciPricing(
  draft: TasciPricingDraft,
): TasciModelPricing | undefined {
  if (!draft.enabled) return undefined;
  const currency = draft.currency.trim().toUpperCase();
  return {
    catalogVersion: draft.catalogVersion.trim(),
    tokenCount: pricingInteger(draft.tokenCount),
    input: pricingMoney(currency, draft.inputAmount),
    cacheReadInput: draft.cacheReadInputAmount
      ? pricingMoney(currency, draft.cacheReadInputAmount)
      : undefined,
    cacheWriteInput: draft.cacheWriteInputAmount
      ? pricingMoney(currency, draft.cacheWriteInputAmount)
      : undefined,
    output: pricingMoney(currency, draft.outputAmount),
  };
}

/** Returns the first validation failure for optional model pricing. */
export function validateTasciPricingDraft(
  draft: TasciPricingDraft,
): string | undefined {
  if (!draft.enabled) return undefined;
  if (!draft.catalogVersion.trim()) return "Pricing version is required.";
  if (!positiveInteger(draft.tokenCount)) {
    return "Pricing token count must be a positive integer.";
  }
  if (!/^[A-Z]{3}$/.test(draft.currency.trim().toUpperCase())) {
    return "Pricing currency must be a three-letter currency code.";
  }
  if (!nonNegativeInteger(draft.inputAmount)) {
    return "Input price must be a non-negative integer.";
  }
  if (draft.cacheReadInputAmount && !nonNegativeInteger(draft.cacheReadInputAmount)) {
    return "Cache-read input price must be a non-negative integer.";
  }
  if (draft.cacheWriteInputAmount && !nonNegativeInteger(draft.cacheWriteInputAmount)) {
    return "Cache-write input price must be a non-negative integer.";
  }
  if (!nonNegativeInteger(draft.outputAmount)) {
    return "Output price must be a non-negative integer.";
  }
  return undefined;
}

function pricingMoney(
  currency: string,
  amount: string,
): TasciModelPricing["input"] {
  return { currency, amount: pricingInteger(amount) };
}

function pricingInteger(value: string): TasciModelPricing["tokenCount"] {
  return Number(value) as TasciModelPricing["tokenCount"];
}

function positiveInteger(value: string): boolean {
  const number = Number(value);
  return Number.isSafeInteger(number) && number > 0;
}

function nonNegativeInteger(value: string): boolean {
  const number = Number(value);
  return value !== "" && Number.isSafeInteger(number) && number >= 0;
}
