export type StackId =
  | "javascript"
  | "python"
  | "rust"
  | "go"
  | "jvm"
  | "cpp"
  | "dotnet"
  | "php"
  | "ruby";

export type WorkspaceFeatureId =
  | "docker"
  | "podman"
  | "virtualization"
  | "usb"
  | "nixDaemon";

export type NetworkMode = "standard" | "restricted";

export type DeveloperToolId = "mise";

export type DeveloperServiceId = "github" | "gitlab";

export interface DeveloperServiceDraft {
  enabled: boolean;
  token: string;
}

export interface WorkspaceRepositoryDraft {
  id: string;
  source: string;
  path: string;
  branch: string;
}

export interface WorkspaceCreationDraft {
  name: string;
  cores: string;
  memory: string;
  disk: string;
  repositories: WorkspaceRepositoryDraft[];
  stacks: StackId[];
  developerTools: DeveloperToolId[];
  developerServices: Record<DeveloperServiceId, DeveloperServiceDraft>;
  features: Record<WorkspaceFeatureId, boolean>;
  networkMode: NetworkMode;
  allowedHosts: string;
  hostPorts: string;
  additionalPackages: string;
}

export interface StackOption {
  id: StackId;
  label: string;
  description: string;
  packages: readonly string[];
  note?: string;
  externalSource?: boolean;
}

export interface FeatureOption {
  id: WorkspaceFeatureId;
  label: string;
  description: string;
  note?: string;
}

export interface DeveloperToolOption {
  id: DeveloperToolId;
  label: string;
  description: string;
  note?: string;
  externalSource?: boolean;
}

export interface DeveloperServiceOption {
  id: DeveloperServiceId;
  label: string;
  cli: string;
  description: string;
  package: string;
  tokenField: "githubToken" | "gitlabToken";
  tokenEnvironmentVariable: string;
  host: string;
  header: string;
  placeholder: string;
  secretName: string;
}

export interface WorkspaceCreationSecret {
  providerName: string;
  secretName: string;
  value: string;
}

export interface WorkspaceCreationDefinition {
  configToml: string;
  dockerfile: string;
  agentsMd: string;
  packages: string[];
  repositories: Array<{ source: string; path: string; branch?: string }>;
  automaticallyAllowedHosts: string[];
  initialSecrets: WorkspaceCreationSecret[];
}

export interface WorkspaceCreationResult {
  definition?: WorkspaceCreationDefinition;
  errors: Partial<Record<WorkspaceCreationField, string>>;
  repositoryErrors: Record<string, { source?: string; path?: string; branch?: string }>;
}

export type WorkspaceCreationField =
  | "name"
  | "cores"
  | "memory"
  | "disk"
  | "githubToken"
  | "gitlabToken"
  | "allowedHosts"
  | "hostPorts"
  | "additionalPackages";

export const STACK_OPTIONS: readonly StackOption[] = [
  {
    id: "javascript",
    label: "JavaScript & TypeScript",
    description: "Node.js, npm, pnpm, and Corepack",
    packages: ["nodejs", "npm", "node-corepack"],
    note: "Installs pnpm 10 from npm for compatibility with Debian 13's Node.js 20.",
    externalSource: true,
  },
  {
    id: "python",
    label: "Python",
    description: "Python, pip, uv, and virtual environments",
    packages: ["python3", "python3-pip", "python3-venv"],
    note: "Copies uv and uvx from Astral's official versioned image.",
    externalSource: true,
  },
  {
    id: "rust",
    label: "Rust",
    description: "Rust, Cargo, and rustup",
    packages: ["build-essential", "pkg-config", "libssl-dev"],
    note: "Installs the official rustup toolchain under the non-root workspace user.",
    externalSource: true,
  },
  {
    id: "go",
    label: "Go",
    description: "Go compiler and standard tooling",
    packages: ["golang-go"],
  },
  {
    id: "jvm",
    label: "Java & JVM",
    description: "Default JDK and Maven",
    packages: ["default-jdk", "maven"],
    note: "Use a project wrapper when a specific Gradle version is required.",
  },
  {
    id: "cpp",
    label: "C & C++",
    description: "GCC, G++, CMake, Ninja, and GDB",
    packages: ["build-essential", "cmake", "ninja-build", "gdb", "pkg-config"],
  },
  {
    id: "dotnet",
    label: ".NET",
    description: ".NET 10 SDK",
    packages: [],
    note: "Adds Microsoft's Debian repository during the image build. Available on x86-64 and ARM64.",
    externalSource: true,
  },
  {
    id: "php",
    label: "PHP",
    description: "PHP CLI, common extensions, and Composer",
    packages: ["php-cli", "php-mbstring", "php-xml", "composer"],
  },
  {
    id: "ruby",
    label: "Ruby",
    description: "Ruby, development headers, and Bundler",
    packages: ["ruby-full", "ruby-dev", "bundler"],
  },
] as const;

export const FEATURE_OPTIONS: readonly FeatureOption[] = [
  {
    id: "docker",
    label: "Docker",
    description: "Run a confined Docker daemon inside every pod.",
  },
  {
    id: "podman",
    label: "Podman",
    description: "Run rootless containers with persistent storage.",
    note: "Container storage persists and is shared across pods.",
  },
  {
    id: "virtualization",
    label: "Nested virtualization",
    description: "Expose /dev/kvm for local VMs and emulators. Requires host support.",
    note: "Requires virtualization support on the Tascarrel host.",
  },
  {
    id: "usb",
    label: "USB forwarding",
    description: "Attach selected host USB devices to pods on Linux hosts.",
    note: "Available when the Tascarrel host runs Linux.",
  },
  {
    id: "nixDaemon",
    label: "Nix daemon",
    description: "Share a persistent Nix store and daemon across pods.",
  },
] as const;

export const DEVELOPER_TOOL_OPTIONS: readonly DeveloperToolOption[] = [
  {
    id: "mise",
    label: "mise",
    description: "Manage project tool versions, environment variables, and tasks",
    note: "Installs mise under the non-root workspace user and shares its toolchains across pods.",
    externalSource: true,
  },
] as const;

export const DEVELOPER_SERVICE_OPTIONS: readonly DeveloperServiceOption[] = [
  {
    id: "github",
    label: "GitHub CLI",
    cli: "gh",
    description: "Use gh for pull requests, issues, releases, and API access.",
    package: "gh",
    tokenField: "githubToken",
    tokenEnvironmentVariable: "GH_TOKEN",
    host: "api.github.com",
    header: "authorization",
    placeholder: "tascarrel-github-read-token",
    secretName: "GITHUB_TOKEN",
  },
  {
    id: "gitlab",
    label: "GitLab CLI",
    cli: "glab",
    description: "Use glab for merge requests, issues, releases, and API access.",
    package: "glab",
    tokenField: "gitlabToken",
    tokenEnvironmentVariable: "GITLAB_TOKEN",
    host: "gitlab.com",
    header: "private-token",
    placeholder: "tascarrel-gitlab-read-token",
    secretName: "GITLAB_TOKEN",
  },
] as const;

const BASE_PACKAGES = [
  "ca-certificates",
  "curl",
  "git",
  "jq",
  "openssh-client",
  "procps",
  "ripgrep",
] as const;

const MICROSOFT_REPOSITORY_SHA256 =
  "d0c2f69250c6ce0d4c6220b142f999d039a3c560af7f980b943687d106ca8e38";
const PNPM_VERSION = "10.34.5";
const RUSTUP_VERSION = "1.29.0";
const RUSTUP_INIT_SHA256_AMD64 =
  "4acc9acc76d5079515b46346a485974457b5a79893cfb01112423c89aeb5aa10";
const RUSTUP_INIT_SHA256_ARM64 =
  "9732d6c5e2a098d3521fca8145d826ae0aaa067ef2385ead08e6feac88fa5792";
const UV_VERSION = "0.11.32";
const WORKSPACE_NAME_PATTERN = /^[A-Za-z0-9_-]{1,64}$/;
const PACKAGE_PATTERN = /^[a-z0-9][a-z0-9+.-]*(?::[a-z0-9]+)?$/;

export function defaultWorkspaceCreationDraft(): WorkspaceCreationDraft {
  return {
    name: "",
    cores: "",
    memory: "",
    disk: "",
    repositories: [],
    stacks: [],
    developerTools: [],
    developerServices: {
      github: { enabled: false, token: "" },
      gitlab: { enabled: false, token: "" },
    },
    features: {
      docker: false,
      podman: false,
      virtualization: false,
      usb: false,
      nixDaemon: false,
    },
    networkMode: "standard",
    allowedHosts: "",
    hostPorts: "",
    additionalPackages: "",
  };
}

export function createWorkspaceDefinition(
  draft: WorkspaceCreationDraft,
): WorkspaceCreationResult {
  const errors: WorkspaceCreationResult["errors"] = {};
  const repositoryErrors: WorkspaceCreationResult["repositoryErrors"] = {};
  const name = draft.name.trim();
  if (!name) {
    errors.name = "Enter a workspace name.";
  } else if (!WORKSPACE_NAME_PATTERN.test(name)) {
    errors.name = "Use 1–64 letters, numbers, underscores, or hyphens.";
  }

  const cores = draft.cores.trim();
  if (cores && (!/^[1-9][0-9]*$/.test(cores) || Number(cores) > 65_535)) {
    errors.cores = "Enter a positive number of CPU cores.";
  }

  const memory = draft.memory.trim();
  if (memory) {
    const memoryBytes = parseBinarySize(memory);
    if (
      memoryBytes === undefined
      || memoryBytes / (1024 ** 2) > 4_294_967_295
    ) {
      errors.memory = 'Use a supported size such as "8G" or "8192MiB".';
    }
  }

  const disk = draft.disk.trim();
  if (disk) {
    const diskBytes = parseBinarySize(disk);
    if (diskBytes === undefined) {
      errors.disk = 'Use a size such as "100G" or "512GiB".';
    } else if (diskBytes < 256 * 1024 * 1024) {
      errors.disk = "Disk size must be at least 256 MiB.";
    }
  }

  const customPackages = splitValues(draft.additionalPackages);
  const invalidPackage = customPackages.find((packageName) => !PACKAGE_PATTERN.test(packageName));
  if (invalidPackage) {
    errors.additionalPackages = `"${invalidPackage}" is not a valid Debian package name.`;
  }

  const repositories = draft.repositories.map((repository) => ({
    id: repository.id,
    source: repository.source.trim(),
    path: repository.path.trim() || inferRepositoryPath(repository.source),
    branch: repository.branch.trim(),
  }));
  for (const repository of repositories) {
    const repositoryError: { source?: string; path?: string; branch?: string } = {};
    if (!isRepositorySource(repository.source)) {
      repositoryError.source = repository.source
        ? "Enter a valid Git source without control characters."
        : "Enter a Git source.";
    }
    if (!isRepositoryPath(repository.path)) {
      repositoryError.path = repository.path
        ? "Use a normalized relative path below /workspace."
        : "Enter a destination or use a source ending in a repository name.";
    }
    if (repository.branch && !isRepositoryBranch(repository.branch)) {
      repositoryError.branch = "Enter a valid short Git branch name.";
    }
    if (repositoryError.source || repositoryError.path || repositoryError.branch) {
      repositoryErrors[repository.id] = repositoryError;
    }
  }
  for (let leftIndex = 0; leftIndex < repositories.length; leftIndex += 1) {
    const left = repositories[leftIndex];
    if (!isRepositoryPath(left.path)) continue;
    for (let rightIndex = leftIndex + 1; rightIndex < repositories.length; rightIndex += 1) {
      const right = repositories[rightIndex];
      if (!isRepositoryPath(right.path) || !repositoryPathsOverlap(left.path, right.path)) continue;
      repositoryErrors[left.id] = {
        ...repositoryErrors[left.id],
        path: "Repository destinations must be unique and may not overlap.",
      };
      repositoryErrors[right.id] = {
        ...repositoryErrors[right.id],
        path: "Repository destinations must be unique and may not overlap.",
      };
    }
  }

  for (const service of DEVELOPER_SERVICE_OPTIONS) {
    if (
      draft.developerServices[service.id].enabled
      && !draft.developerServices[service.id].token.trim()
    ) {
      errors[service.tokenField] = `Enter a read-only token for ${service.label}.`;
    }
  }

  const requestedHosts = draft.networkMode === "restricted"
    ? splitValues(draft.allowedHosts)
    : [];
  if (draft.networkMode === "restricted") {
    const invalidHost = requestedHosts.find((host) => !isHostPattern(host));
    if (invalidHost) {
      errors.allowedHosts = `"${invalidHost}" is not a valid hostname pattern.`;
    }
  }

  const hostPorts = splitValues(draft.hostPorts);
  const invalidPort = hostPorts.find((port) => !isHostPort(port));
  if (invalidPort) {
    errors.hostPorts = `"${invalidPort}" must be a port or host:pod port mapping.`;
  } else if (hostPorts.length > 64) {
    errors.hostPorts = "At most 64 host services can be mapped.";
  } else {
    const podPorts = hostPorts.map((mapping) => mapping.split(":").at(-1));
    const duplicatePodPort = podPorts.find((port, index) => podPorts.indexOf(port) !== index);
    if (duplicatePodPort) {
      errors.hostPorts = `Pod-side port ${duplicatePodPort} is mapped more than once.`;
    }
  }

  if (Object.keys(errors).length > 0 || Object.keys(repositoryErrors).length > 0) {
    return { errors, repositoryErrors };
  }

  const selectedStacks = STACK_OPTIONS.filter((option) => draft.stacks.includes(option.id));
  const selectedDeveloperTools = DEVELOPER_TOOL_OPTIONS.filter(
    (option) => draft.developerTools.includes(option.id),
  );
  const selectedServices = DEVELOPER_SERVICE_OPTIONS.filter(
    (option) => draft.developerServices[option.id].enabled,
  );
  const packages = [
    ...BASE_PACKAGES,
    ...selectedStacks.flatMap((option) => option.packages),
    ...selectedServices.map((option) => option.package),
    ...customPackages,
  ].toSorted().filter((packageName, index, values) => values[index - 1] !== packageName);
  const automaticallyAllowedHosts = [
    "deb.debian.org",
    ...(draft.stacks.includes("javascript") ? ["registry.npmjs.org"] : []),
    ...(draft.stacks.includes("python")
      ? ["ghcr.io", "pkg-containers.githubusercontent.com"]
      : []),
    ...(draft.stacks.includes("rust") ? ["static.rust-lang.org"] : []),
    ...(draft.stacks.includes("dotnet") ? ["packages.microsoft.com"] : []),
    ...(draft.developerTools.includes("mise")
      ? ["mise.run", "github.com", "release-assets.githubusercontent.com"]
      : []),
    ...selectedServices.map((service) => service.host),
  ];
  const initialSecrets = selectedServices.map((service) => ({
    providerName: "services",
    secretName: service.secretName,
    value: draft.developerServices[service.id].token.trim(),
  }));

  return {
    errors,
    repositoryErrors,
    definition: {
      configToml: renderConfigToml(
        draft,
        repositories,
        requestedHosts,
        hostPorts,
        automaticallyAllowedHosts,
        selectedServices,
      ),
      dockerfile: renderDockerfile(packages, {
        dotnet: draft.stacks.includes("dotnet"),
        javascript: draft.stacks.includes("javascript"),
        mise: selectedDeveloperTools.some((option) => option.id === "mise"),
        python: draft.stacks.includes("python"),
        rust: draft.stacks.includes("rust"),
      }),
      agentsMd: renderAgentsMarkdown(draft.features.nixDaemon),
      packages,
      repositories: repositories.map(({ source, path, branch }) => ({
        source,
        path,
        ...(branch ? { branch } : {}),
      })),
      automaticallyAllowedHosts: draft.networkMode === "restricted"
        ? automaticallyAllowedHosts
        : [],
      initialSecrets,
    },
  };
}

function renderConfigToml(
  draft: WorkspaceCreationDraft,
  repositories: Array<{ source: string; path: string; branch: string }>,
  requestedHosts: string[],
  hostPorts: string[],
  automaticallyAllowedHosts: string[],
  selectedServices: readonly DeveloperServiceOption[],
): string {
  const sections: string[] = [];
  const vmEntries = [
    ...(draft.cores.trim() ? [`cores = ${draft.cores.trim()}`] : []),
    ...(draft.memory.trim() ? [`memory = ${tomlString(draft.memory.trim())}`] : []),
    ...(draft.disk.trim() ? [`disk = ${tomlString(draft.disk.trim())}`] : []),
  ];
  if (vmEntries.length > 0) sections.push(`[vm]\n${vmEntries.join("\n")}`);

  const featureEntries = [
    ...(draft.features.docker ? ["docker = true"] : []),
    ...(draft.features.podman ? ["podman = true"] : []),
    ...(draft.features.virtualization ? ["virtualization = true"] : []),
    ...(draft.features.usb ? ["usb = true"] : []),
  ];
  if (featureEntries.length > 0) sections.push(`[features]\n${featureEntries.join("\n")}`);
  if (draft.features.nixDaemon) sections.push("[nix]\ndaemon = true");

  for (const repository of repositories) {
    sections.push([
      `[repos.${tomlString(repository.path)}]`,
      `source = ${tomlString(repository.source)}`,
      ...(repository.branch ? [`branch = ${tomlString(repository.branch)}`] : []),
    ].join("\n"));
  }

  const environmentEntries = [
    ...(draft.developerTools.includes("mise")
      ? [
          'MISE_CONFIG_DIR = "/home/develop/.mise/config"',
          'MISE_CACHE_DIR = "/home/develop/.mise/cache"',
          'MISE_STATE_DIR = "/home/develop/.mise/state"',
          'MISE_DATA_DIR = "/home/develop/.mise/data"',
          'MISE_TRUSTED_CONFIG_PATHS = "/"',
        ]
      : []),
    ...selectedServices.map(
      (service) =>
        `${service.tokenEnvironmentVariable} = ${tomlString(service.placeholder)}`,
    ),
  ];
  if (environmentEntries.length > 0) {
    sections.push([
      "[env]",
      ...environmentEntries,
    ].join("\n"));
  }

  if (selectedServices.length > 0) {
    sections.push('[secrets.providers.services]\nkind = "sops"\nfile = "secrets.json"');
  }

  const caches = [
    ...(draft.developerTools.includes("mise")
      ? [{ name: "mise", path: "~/.mise" }]
      : []),
    ...(draft.features.podman
      ? [{ name: "containers", path: "~/.local/share/containers" }]
      : []),
  ];
  for (const cache of caches) {
    sections.push([
      "[[caches]]",
      `name = ${tomlString(cache.name)}`,
      `path = ${tomlString(cache.path)}`,
    ].join("\n"));
  }

  const networkEntries: string[] = [];
  if (draft.networkMode === "restricted") {
    const allowedHosts = [...automaticallyAllowedHosts, ...requestedHosts]
      .map((host) => host.toLowerCase())
      .toSorted()
      .filter((host, index, values) => values[index - 1] !== host);
    networkEntries.push('default = "deny"');
    if (allowedHosts.length > 0) {
      networkEntries.push(`allow-hosts = [${allowedHosts.map(tomlString).join(", ")}]`);
    }
  }
  if (hostPorts.length > 0) {
    networkEntries.push(
      `host-ports = [${hostPorts.map((port) => port.includes(":") ? tomlString(port) : port).join(", ")}]`,
    );
  }
  if (networkEntries.length > 0) sections.push(`[network]\n${networkEntries.join("\n")}`);
  for (const service of selectedServices) {
    sections.push([
      "[[network.secret-injection]]",
      `host = ${tomlString(service.host)}`,
      'methods = ["GET", "HEAD", "POST"]',
      `header = ${tomlString(service.header)}`,
      `placeholder = ${tomlString(service.placeholder)}`,
      `secret = ${tomlString(`services.${service.secretName}`)}`,
    ].join("\n"));
  }

  return sections.length > 0 ? `${sections.join("\n\n")}\n` : "# Tascarrel defaults\n";
}

function renderDockerfile(
  packages: string[],
  options: {
    dotnet: boolean;
    javascript: boolean;
    mise: boolean;
    python: boolean;
    rust: boolean;
  },
): string {
  const packageLines = packages.map((packageName) => `      ${packageName} \\`).join("\n");
  const sections = [
    "FROM docker.io/library/debian:trixie",
    [
      "RUN apt-get update \\",
      " && apt-get install -y --no-install-recommends \\",
      packageLines,
      " && rm -rf /var/lib/apt/lists/*",
    ].join("\n"),
  ];

  if (options.python) {
    sections.push(
      `COPY --from=ghcr.io/astral-sh/uv:${UV_VERSION} /uv /uvx /usr/local/bin/`,
    );
  }

  if (options.javascript) {
    sections.push([
      `ARG PNPM_VERSION=${PNPM_VERSION}`,
      'RUN npm install --global "pnpm@${PNPM_VERSION}" \\',
      " && npm cache clean --force \\",
      " && pnpm --version",
    ].join("\n"));
  }

  if (options.dotnet) {
    sections.push([
      `ARG MICROSOFT_REPOSITORY_SHA256=${MICROSOFT_REPOSITORY_SHA256}`,
      "RUN set -eux; \\",
      '    architecture="$(dpkg --print-architecture)"; \\',
      '    case "$architecture" in amd64|arm64) ;; *) echo "Unsupported architecture: $architecture" >&2; exit 1 ;; esac; \\',
      '    repository_package="$(mktemp)"; \\',
      '    curl -fsSL "https://packages.microsoft.com/config/debian/13/packages-microsoft-prod.deb" -o "$repository_package"; \\',
      '    echo "$MICROSOFT_REPOSITORY_SHA256  $repository_package" | sha256sum --check -; \\',
      '    dpkg -i "$repository_package"; \\',
      '    rm -f "$repository_package"; \\',
      "    apt-get update; \\",
      "    apt-get install -y --no-install-recommends dotnet-sdk-10.0; \\",
      "    rm -rf /var/lib/apt/lists/*",
    ].join("\n"));
  }

  if (options.rust || options.mise) {
    sections.push("RUN useradd --create-home --uid 1000 develop", "USER develop");
    const pathEntries = [
      ...(options.mise ? ["/home/develop/.local/bin"] : []),
      ...(options.rust ? ["/home/develop/.cargo/bin"] : []),
      "${PATH}",
    ];
    sections.push(`ENV PATH="${pathEntries.join(":")}"`);
  }

  if (options.mise) {
    sections.push([
      "RUN curl --proto '=https' --tlsv1.2 -fsSL https://mise.run | sh \\",
      ' && mkdir -p "$HOME/.local/share/zsh/site-functions" \\',
      ' && mise completion zsh > "$HOME/.local/share/zsh/site-functions/_mise" \\',
      " && mise --version",
    ].join("\n"));
  }

  if (options.rust) {
    sections.push([
        `ARG RUSTUP_VERSION=${RUSTUP_VERSION}`,
        `ARG RUSTUP_INIT_SHA256_AMD64=${RUSTUP_INIT_SHA256_AMD64}`,
        `ARG RUSTUP_INIT_SHA256_ARM64=${RUSTUP_INIT_SHA256_ARM64}`,
        "RUN set -eux; \\",
        '    architecture="$(dpkg --print-architecture)"; \\',
        '    case "$architecture" in \\',
        '      amd64) target="x86_64-unknown-linux-gnu"; checksum="$RUSTUP_INIT_SHA256_AMD64" ;; \\',
        '      arm64) target="aarch64-unknown-linux-gnu"; checksum="$RUSTUP_INIT_SHA256_ARM64" ;; \\',
        '      *) echo "Unsupported architecture: $architecture" >&2; exit 1 ;; \\',
        "    esac; \\",
        '    temporary_directory="$(mktemp -d)"; \\',
        '    installer="${temporary_directory}/rustup-init"; \\',
        '    curl --proto \'=https\' --tlsv1.2 -fsSL "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/${target}/rustup-init" --output "$installer"; \\',
        '    echo "$checksum  $installer" | sha256sum --check -; \\',
        '    chmod +x "$installer"; \\',
        '    "$installer" -y --no-modify-path --profile default --default-toolchain stable; \\',
        '    rm -rf "$temporary_directory"; \\',
        "    rustup --version; \\",
        "    cargo --version; \\",
        "    rustc --version",
      ].join("\n"));
  }

  sections.push('ENV LANG="C.UTF-8"', 'ENV LC_ALL="C.UTF-8"', "WORKDIR /workspace");
  return `${sections.join("\n\n")}\n`;
}

function renderAgentsMarkdown(nixDaemon: boolean): string {
  const sections = [
    [
      "This workspace contains multiple repositories. AGENTS.md files in repositories",
      "ALWAYS take priority over the instructions in this file. The user's explicit",
      "instructions ALWAYS take priority over the instructions in any AGENTS.md. If",
      "you encounter a conflict, point it out to the user.",
    ].join("\n"),
    [
      "Find the repositories relevant to a user's request, then work within them. Use",
      "the other repositories as additional context when required.",
    ].join("\n"),
    ...(nixDaemon
      ? ["Use `nix` to run ad-hoc tools that are not installed."]
      : []),
    [
      "When starting an HTTP dev server (e.g., Vite, Astro) expose it through:",
      "",
      "`podctl http publish --title TITLE PORT`",
      "",
      "Choose a short title (4 words max) that describes what's running.",
    ].join("\n"),
  ];
  return `${sections.join("\n\n")}\n`;
}

function splitValues(value: string): string[] {
  return value.split(/[\s,]+/).map((entry) => entry.trim()).filter(Boolean);
}

function inferRepositoryPath(source: string): string {
  const normalized = source.trim().replaceAll("\\", "/").replace(/\/+$/, "");
  const finalSegment = normalized.split(/[/:]/).at(-1) ?? "";
  return finalSegment.replace(/\.git$/i, "");
}

function isRepositorySource(value: string): boolean {
  return value.length > 0
    && value.length <= 4096
    && !value.startsWith("-")
    && !/[\u0000-\u001F\u007F]/.test(value);
}

function isRepositoryPath(value: string): boolean {
  if (!value || value.length > 1024 || value.startsWith("/") || /[\u0000-\u001F\u007F]/.test(value)) {
    return false;
  }
  return value.split("/").every((segment) => segment && segment !== "." && segment !== "..");
}

function isRepositoryBranch(value: string): boolean {
  return value.length <= 1024
    && !value.startsWith("-")
    && !value.startsWith("/")
    && !value.endsWith("/")
    && !value.endsWith(".")
    && !value.includes("..")
    && !value.includes("@{")
    && !value.includes("//")
    && !/[\u0000-\u0020\u007F]/.test(value)
    && !["~", "^", ":", "?", "*", "[", "\\"].some((character) => value.includes(character))
    && value.split("/").every((component) =>
      component.length > 0
      && !component.startsWith(".")
      && !component.toLowerCase().endsWith(".lock")
    );
}

function repositoryPathsOverlap(left: string, right: string): boolean {
  const leftParts = left.split("/");
  const rightParts = right.split("/");
  const sharedLength = Math.min(leftParts.length, rightParts.length);
  return leftParts.slice(0, sharedLength).every((part, index) => part === rightParts[index]);
}

function isHostPort(value: string): boolean {
  const parts = value.split(":");
  if (parts.length > 2 || parts.some((part) => !/^[0-9]+$/.test(part))) return false;
  return parts.every((part) => {
    const port = Number(part);
    return port >= 1 && port <= 65_535;
  });
}

function isHostPattern(value: string): boolean {
  const hostname = value.startsWith("*.") ? value.slice(2) : value;
  return hostname.length > 0
    && hostname.length <= 253
    && hostname.split(".").every(
      (label) => label.length > 0 && /^[A-Za-z0-9-]+$/.test(label),
    );
}

function parseBinarySize(value: string): number | undefined {
  const match = /^([1-9][0-9]*)(M|G|T|MiB|GiB|TiB)$/i.exec(value);
  if (!match) return undefined;
  const multipliers: Record<string, number> = {
    M: 1024 ** 2,
    MIB: 1024 ** 2,
    G: 1024 ** 3,
    GIB: 1024 ** 3,
    T: 1024 ** 4,
    TIB: 1024 ** 4,
  };
  const size = Number(match[1]) * multipliers[match[2].toUpperCase()];
  return Number.isSafeInteger(size) ? size : undefined;
}

function tomlString(value: string): string {
  return JSON.stringify(value);
}
