import assert from "node:assert/strict";
import test from "node:test";

import { podFileTarget } from "./podFileLinks.ts";

test("resolves absolute agent file links relative to the pod workspace", () => {
  assert.deepEqual(
    podFileTarget("/workspace/tascarrel/frontend/src/App.tsx:42"),
    { root: { tag: "Workspace" }, path: "tascarrel/frontend/src/App.tsx", line: 42 },
  );
  assert.deepEqual(
    podFileTarget("file:///workspace/docs/Guide%20One.md#L7"),
    { root: { tag: "Workspace" }, path: "docs/Guide One.md", line: 7 },
  );
});

test("resolves links in rendered Markdown from the containing file", () => {
  assert.deepEqual(
    podFileTarget("../images/diagram.png", {
      root: { tag: "Workspace" },
      path: "docs/guides/setup.md",
    }),
    { root: { tag: "Workspace" }, path: "docs/images/diagram.png" },
  );
  assert.deepEqual(
    podFileTarget("./details.md#L4-L9", {
      root: { tag: "Workspace" },
      path: "docs/README.md",
    }),
    { root: { tag: "Workspace" }, path: "docs/details.md", line: 4 },
  );
});

test("resolves absolute and relative links inside configured share roots", () => {
  const share = { tag: "Share", name: "design_assets" } as const;
  assert.deepEqual(
    podFileTarget("file:///mnt/design_assets/specs/layout.pdf#L3"),
    { root: share, path: "specs/layout.pdf", line: 3 },
  );
  assert.deepEqual(
    podFileTarget("../images/diagram.png", {
      root: share,
      path: "specs/guides/setup.md",
    }),
    { root: share, path: "specs/images/diagram.png" },
  );
});

test("leaves remote and out-of-root links outside the file preview", () => {
  assert.equal(podFileTarget("https://example.com/file.txt"), undefined);
  assert.equal(podFileTarget("/etc/passwd"), undefined);
  assert.equal(podFileTarget("/mnt/invalid.name/file.txt"), undefined);
  assert.equal(podFileTarget("../../secret.txt", {
    root: { tag: "Workspace" },
    path: "README.md",
  }), undefined);
  assert.equal(podFileTarget("child.txt", {
    root: { tag: "Share", name: "design_assets" },
    path: "../README.md",
  }), undefined);
});
