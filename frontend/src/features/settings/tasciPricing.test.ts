import assert from "node:assert/strict";
import test from "node:test";

import type { chats } from "../../api/generated/index.ts";
import {
  tasciPricing,
  tasciPricingDraft,
  validateTasciPricingDraft,
} from "./tasciPricing.ts";

test("Tasci pricing drafts preserve required and optional token rates", () => {
  const pricing: chats.ChatModelPricing = {
    catalogVersion: "provider:2026-08-01",
    tokenCount: 1_000_000 as chats.ChatModelPricing["tokenCount"],
    input: { currency: "USD", amount: pricingAmount(125) },
    cacheReadInput: { currency: "USD", amount: pricingAmount(25) },
    cacheWriteInput: { currency: "USD", amount: pricingAmount(150) },
    output: { currency: "USD", amount: pricingAmount(500) },
  };

  const draft = tasciPricingDraft(pricing);

  assert.equal(validateTasciPricingDraft(draft), undefined);
  assert.deepEqual(tasciPricing(draft), pricing);
});

test("Tasci pricing validation rejects incomplete or unsafe rates", () => {
  const draft = tasciPricingDraft();
  draft.enabled = true;
  draft.inputAmount = "1.5";
  draft.outputAmount = "9007199254740992";

  assert.equal(
    validateTasciPricingDraft(draft),
    "Input price must be a non-negative integer.",
  );

  draft.inputAmount = "0";
  assert.equal(
    validateTasciPricingDraft(draft),
    "Output price must be a non-negative integer.",
  );
});

function pricingAmount(value: number): chats.ChatModelPricing["input"]["amount"] {
  return value as chats.ChatModelPricing["input"]["amount"];
}
