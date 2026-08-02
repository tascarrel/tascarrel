import assert from "node:assert/strict";
import test from "node:test";

import type { chats } from "../../../api/generated/index.ts";
import { presentContextUsage } from "./contextUsage.ts";

test("presents reported context usage with its effective capacity", () => {
  assert.deepEqual(
    presentContextUsage({
      usedTokens: 42_000,
      contextWindowTokens: 200_000,
      accuracy: "Reported",
      observedAt: "2026-08-02T10:00:00Z",
    } as unknown as chats.ChatContextUsage),
    {
      description: "Current context: 42,000 of 200,000 tokens",
      value: "42K / 200K",
    },
  );
});

test("marks estimates and supports an unknown capacity", () => {
  assert.deepEqual(
    presentContextUsage({
      usedTokens: 42_000,
      accuracy: "Estimated",
      observedAt: "2026-08-02T10:00:00Z",
    } as unknown as chats.ChatContextUsage),
    {
      description: "Estimated current context: 42,000 tokens",
      value: "~42K",
    },
  );
});

test("presents unavailable context usage as N/A", () => {
  assert.deepEqual(presentContextUsage(undefined), {
    description: "Current context usage is unavailable",
    value: "N/A",
  });
});
