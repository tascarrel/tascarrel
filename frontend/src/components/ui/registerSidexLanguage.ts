import {
  registerCustomLanguage,
  RegisteredCustomLanguages,
} from "@pierre/diffs";

import { SIDEX_LANGUAGE } from "./sidexLanguage.ts";

if (!RegisteredCustomLanguages.has(SIDEX_LANGUAGE.name)) {
  registerCustomLanguage(
    SIDEX_LANGUAGE.name,
    async () => ({ default: [SIDEX_LANGUAGE] }),
    ["sidex"],
  );
}
