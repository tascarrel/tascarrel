import assert from "node:assert/strict";
import test from "node:test";

import {
  createWorkspaceDefinition,
  defaultWorkspaceCreationDraft,
} from "./model.ts";

test("creates a minimal definition without assuming a stack or capability", () => {
  const result = createWorkspaceDefinition({
    ...defaultWorkspaceCreationDraft(),
    name: "demo",
  });

  assert.deepEqual(result.errors, {});
  assert.equal(result.definition?.configToml, "# Tascarrel defaults\n");
  assert.match(result.definition?.dockerfile ?? "", /^FROM docker\.io\/library\/debian:trixie/m);
  assert.doesNotMatch(result.definition?.dockerfile ?? "", /podman|zsh|starship/i);
  assert.match(result.definition?.agentsMd ?? "", /AGENTS\.md files in repositories/);
  assert.match(result.definition?.agentsMd ?? "", /podctl http publish/);
  assert.doesNotMatch(result.definition?.agentsMd ?? "", /Use `nix`/);
});

test("composes stacks, capabilities, resources, and restricted networking", () => {
  const result = createWorkspaceDefinition({
    ...defaultWorkspaceCreationDraft(),
    name: "full-stack",
    cores: "8",
    memory: "16G",
    disk: "200G",
    repositories: [
      {
        id: "api",
        source: "git@github.com:example/api.git",
        path: "product/api",
        branch: "release/next",
      },
      {
        id: "web",
        source: "https://github.com/example/web.git",
        path: "",
        branch: "",
      },
    ],
    stacks: ["javascript", "python", "rust", "dotnet"],
    developerTools: ["mise"],
    features: {
      docker: true,
      podman: false,
      virtualization: true,
      usb: false,
      nixDaemon: true,
    },
    networkMode: "restricted",
    allowedHosts: "github.com *.githubusercontent.com",
    hostPorts: "3000 5432:15432",
    additionalPackages: "jq shellcheck",
  });

  assert.deepEqual(result.errors, {});
  assert.match(result.definition?.configToml ?? "", /\[vm\]\ncores = 8/);
  assert.match(result.definition?.configToml ?? "", /\[features\]\ndocker = true\nvirtualization = true/);
  assert.match(result.definition?.configToml ?? "", /\[nix\]\ndaemon = true/);
  assert.match(result.definition?.configToml ?? "", /default = "deny"/);
  assert.match(result.definition?.configToml ?? "", /"packages\.microsoft\.com"/);
  assert.match(result.definition?.configToml ?? "", /host-ports = \[3000, "5432:15432"\]/);
  assert.match(
    result.definition?.configToml ?? "",
    /\[repos\."product\/api"\]\nsource = "git@github\.com:example\/api\.git"\nbranch = "release\/next"/,
  );
  assert.match(
    result.definition?.configToml ?? "",
    /\[repos\."web"\]\nsource = "https:\/\/github\.com\/example\/web\.git"/,
  );
  assert.deepEqual(result.definition?.repositories, [
    {
      source: "git@github.com:example/api.git",
      path: "product/api",
      branch: "release/next",
    },
    {
      source: "https://github.com/example/web.git",
      path: "web",
    },
  ]);
  assert.match(result.definition?.dockerfile ?? "", /node-corepack/);
  assert.match(result.definition?.dockerfile ?? "", /pnpm@\$[{]PNPM_VERSION[}]/);
  assert.match(result.definition?.dockerfile ?? "", /python3-venv/);
  assert.match(
    result.definition?.dockerfile ?? "",
    /COPY --from=ghcr\.io\/astral-sh\/uv:0\.\d+\.\d+ \/uv \/uvx \/usr\/local\/bin\//,
  );
  assert.match(result.definition?.dockerfile ?? "", /dotnet-sdk-10\.0/);
  assert.doesNotMatch(result.definition?.dockerfile ?? "", /\n      (?:cargo|rustc) \\/);
  assert.match(
    result.definition?.dockerfile ?? "",
    /RUN useradd --create-home --uid 1000 develop\n\nUSER develop/,
  );
  assert.match(
    result.definition?.dockerfile ?? "",
    /static\.rust-lang\.org\/rustup\/archive\/\$\{RUSTUP_VERSION\}\/\$\{target\}\/rustup-init/,
  );
  assert.match(
    result.definition?.dockerfile ?? "",
    /"\$installer" -y --no-modify-path --profile default --default-toolchain stable/,
  );
  assert.match(result.definition?.dockerfile ?? "", /cargo --version/);
  assert.match(
    result.definition?.dockerfile ?? "",
    /ENV PATH="\/home\/develop\/\.local\/bin:\/home\/develop\/\.cargo\/bin:/,
  );
  assert.match(
    result.definition?.dockerfile ?? "",
    /USER develop[\s\S]*curl --proto '=https' --tlsv1\.2 -fsSL https:\/\/mise\.run \| sh/,
  );
  assert.match(
    result.definition?.dockerfile ?? "",
    /mise completion zsh > "\$HOME\/\.local\/share\/zsh\/site-functions\/_mise"/,
  );
  assert.match(result.definition?.dockerfile ?? "", /mise --version/);
  assert.match(result.definition?.dockerfile ?? "", /shellcheck/);
  assert.match(result.definition?.configToml ?? "", /"ghcr\.io"/);
  assert.match(result.definition?.configToml ?? "", /"pkg-containers\.githubusercontent\.com"/);
  assert.match(result.definition?.configToml ?? "", /"registry\.npmjs\.org"/);
  assert.match(result.definition?.configToml ?? "", /"static\.rust-lang\.org"/);
  assert.match(result.definition?.configToml ?? "", /"mise\.run"/);
  assert.match(result.definition?.configToml ?? "", /"github\.com"/);
  assert.match(result.definition?.configToml ?? "", /"release-assets\.githubusercontent\.com"/);
  assert.match(result.definition?.agentsMd ?? "", /Use `nix` to run ad-hoc tools/);
});

test("configures service CLIs with host-injected SOPS secrets", () => {
  const githubToken = "github_pat_read_only_example";
  const gitlabToken = "glpat-read-only-example";
  const result = createWorkspaceDefinition({
    ...defaultWorkspaceCreationDraft(),
    name: "service-tools",
    developerServices: {
      github: { enabled: true, token: githubToken },
      gitlab: { enabled: true, token: gitlabToken },
    },
    networkMode: "restricted",
  });

  assert.deepEqual(result.errors, {});
  assert.match(result.definition?.dockerfile ?? "", /\n      gh \\/);
  assert.match(result.definition?.dockerfile ?? "", /\n      glab \\/);
  assert.match(
    result.definition?.configToml ?? "",
    /\[secrets\.providers\.services\]\nkind = "sops"\nfile = "secrets\.json"/,
  );
  assert.match(result.definition?.configToml ?? "", /GH_TOKEN = "tascarrel-github-read-token"/);
  assert.match(result.definition?.configToml ?? "", /header = "authorization"/);
  assert.match(result.definition?.configToml ?? "", /header = "private-token"/);
  assert.match(result.definition?.configToml ?? "", /methods = \["GET", "HEAD", "POST"\]/);
  assert.match(result.definition?.configToml ?? "", /"api\.github\.com"/);
  assert.match(result.definition?.configToml ?? "", /"gitlab\.com"/);
  assert.doesNotMatch(result.definition?.configToml ?? "", /github_pat_read_only_example/);
  assert.doesNotMatch(result.definition?.dockerfile ?? "", /github_pat_read_only_example/);
  assert.deepEqual(result.definition?.initialSecrets, [
    {
      providerName: "services",
      secretName: "GITHUB_TOKEN",
      value: githubToken,
    },
    {
      providerName: "services",
      secretName: "GITLAB_TOKEN",
      value: gitlabToken,
    },
  ]);
});

test("configures persistent mise and Podman caches independently", () => {
  const miseResult = createWorkspaceDefinition({
    ...defaultWorkspaceCreationDraft(),
    name: "mise-cache",
    developerTools: ["mise"],
    developerServices: {
      github: { enabled: true, token: "github-read-only-example" },
      gitlab: { enabled: false, token: "" },
    },
  });
  const podmanResult = createWorkspaceDefinition({
    ...defaultWorkspaceCreationDraft(),
    name: "podman-cache",
    features: {
      ...defaultWorkspaceCreationDraft().features,
      podman: true,
    },
  });

  assert.match(
    miseResult.definition?.configToml ?? "",
    /\[env\]\nMISE_CONFIG_DIR = "\/home\/develop\/\.mise\/config"\nMISE_CACHE_DIR = "\/home\/develop\/\.mise\/cache"\nMISE_STATE_DIR = "\/home\/develop\/\.mise\/state"\nMISE_DATA_DIR = "\/home\/develop\/\.mise\/data"\nMISE_TRUSTED_CONFIG_PATHS = "\/"/,
  );
  assert.match(
    miseResult.definition?.configToml ?? "",
    /\[\[caches\]\]\nname = "mise"\npath = "~\/\.mise"/,
  );
  assert.equal(miseResult.definition?.configToml.match(/^\[env\]$/gm)?.length, 1);
  assert.match(miseResult.definition?.configToml ?? "", /GH_TOKEN = "tascarrel-github-read-token"/);
  assert.doesNotMatch(miseResult.definition?.configToml ?? "", /name = "containers"/);
  assert.match(
    podmanResult.definition?.configToml ?? "",
    /\[\[caches\]\]\nname = "containers"\npath = "~\/\.local\/share\/containers"/,
  );
  assert.doesNotMatch(podmanResult.definition?.configToml ?? "", /MISE_|name = "mise"/);
});

test("requires a token when a developer service is selected", () => {
  const result = createWorkspaceDefinition({
    ...defaultWorkspaceCreationDraft(),
    name: "missing-token",
    developerServices: {
      github: { enabled: true, token: "  " },
      gitlab: { enabled: false, token: "" },
    },
  });

  assert.equal(result.definition, undefined);
  assert.ok(result.errors.githubToken);
});

test("rejects malformed fields before generating files", () => {
  const result = createWorkspaceDefinition({
    ...defaultWorkspaceCreationDraft(),
    name: "spaces are invalid",
    disk: "128M",
    networkMode: "restricted",
    allowedHosts: "bad_host.example",
    hostPorts: "3000 4000:3000",
    additionalPackages: "valid $(unsafe)",
    repositories: [
      {
        id: "parent",
        source: "https://example.invalid/parent.git",
        path: "src",
        branch: "main",
      },
      {
        id: "nested",
        source: "https://example.invalid/nested.git",
        path: "src/nested",
        branch: "../invalid",
      },
    ],
  });

  assert.equal(result.definition, undefined);
  assert.ok(result.errors.name);
  assert.ok(result.errors.disk);
  assert.ok(result.errors.allowedHosts);
  assert.ok(result.errors.hostPorts);
  assert.ok(result.errors.additionalPackages);
  assert.ok(result.repositoryErrors.parent?.path);
  assert.ok(result.repositoryErrors.nested?.path);
  assert.ok(result.repositoryErrors.nested?.branch);
});
