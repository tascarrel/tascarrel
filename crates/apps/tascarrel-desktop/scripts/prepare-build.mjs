import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const desktopRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = process.env.TASCARREL_WORKSPACE_ROOT
  ? resolve(process.env.TASCARREL_WORKSPACE_ROOT)
  : resolve(desktopRoot, "../../..");
const packageMetadata = JSON.parse(
  await readFile(resolve(desktopRoot, "package.json"), "utf8"),
);
const workspaceManifest = await readFile(resolve(repositoryRoot, "Cargo.toml"), "utf8");
const protocolSource = await readFile(
  resolve(repositoryRoot, "crates/libs/tascarrel-protocol/src/lib.rs"),
  "utf8",
);
const workspaceVersion = workspaceManifest
  .match(/\[workspace\.package\][\s\S]*?\nversion = "([^"]+)"/)?.[1];
const protocolVersion = protocolSource
  .match(/pub const PROTOCOL_VERSION: u16 = (\d+);/)?.[1];

if (!workspaceVersion || workspaceVersion !== packageMetadata.version) {
  throw new Error(
    `Desktop version ${packageMetadata.version} does not match workspace version ${workspaceVersion ?? "unknown"}`,
  );
}
if (!protocolVersion) {
  throw new Error("Could not read the Tascarrel protocol version");
}

const output = resolve(desktopRoot, "dist");
await rm(output, { force: true, recursive: true });
await mkdir(output, { recursive: true });
await writeFile(resolve(output, "protocol-version"), `${protocolVersion}\n`);
