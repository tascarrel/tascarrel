import assert from "node:assert/strict";
import test from "node:test";

import { workspaceFileTarget } from "./workspaceFileLinks.ts";

test("resolves absolute agent file links relative to the pod workspace", () => {
  assert.deepEqual(
    workspaceFileTarget("/workspace/tascarrel/frontend/src/App.tsx:42"),
    { path: "tascarrel/frontend/src/App.tsx", line: 42 },
  );
  assert.deepEqual(
    workspaceFileTarget("file:///workspace/docs/Guide%20One.md#L7"),
    { path: "docs/Guide One.md", line: 7 },
  );
});

test("resolves links in rendered Markdown from the containing file", () => {
  assert.deepEqual(
    workspaceFileTarget("../images/diagram.png", "docs/guides/setup.md"),
    { path: "docs/images/diagram.png" },
  );
  assert.deepEqual(
    workspaceFileTarget("./details.md#L4-L9", "/workspace/docs/README.md"),
    { path: "docs/details.md", line: 4 },
  );
});

test("leaves remote and out-of-workspace links outside the file preview", () => {
  assert.equal(workspaceFileTarget("https://example.com/file.txt"), undefined);
  assert.equal(workspaceFileTarget("/etc/passwd"), undefined);
  assert.equal(workspaceFileTarget("../../secret.txt", "README.md"), undefined);
});
