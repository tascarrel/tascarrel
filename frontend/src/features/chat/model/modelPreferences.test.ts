import assert from "node:assert/strict";
import test from "node:test";

import type { config } from "../../../api/generated/index.ts";
import {
  chatModelPreferences,
  withChatModelPreferences,
} from "./modelPreferences.ts";

test("Tasci model presentation preferences round-trip without changing another harness", () => {
  const settings: config.WorkspaceSettings = {
    chat: {
      harnesses: {
        claudeCode: {
          modelOrder: ["claude-sonnet"],
        },
      },
      tasci: {
        defaultModel: "large",
        endpoints: {},
        models: {},
      },
    },
  };
  const updated = withChatModelPreferences(settings, "Tasci", {
    defaultModel: {
      model: "small",
      options: [],
    },
    modelOrder: ["small", "large"],
    hiddenModels: ["large"],
    favoriteModels: ["small"],
  });

  assert.deepEqual(updated.chat?.harnesses, settings.chat?.harnesses);
  assert.deepEqual(chatModelPreferences(updated, "Tasci"), {
    defaultModel: {
      model: "small",
      options: [],
    },
    modelOrder: ["small", "large"],
    hiddenModels: ["large"],
    favoriteModels: ["small"],
  });
});
