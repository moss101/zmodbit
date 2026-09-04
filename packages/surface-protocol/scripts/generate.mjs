// Regenerates TypeScript bindings from the canonical protobuf schemas.
// Cross-platform on purpose: protoc plugins are resolved from this package's
// node_modules/.bin with the platform shim suffix, and proto files are
// enumerated explicitly (no shell globbing — cmd.exe does not expand globs).
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const pkgRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = resolve(pkgRoot, "..", "..");
const protoRoot = join(repoRoot, "proto");
const outDir = join(pkgRoot, "src", "generated");

const protos = [
  "modbit/protocol/v1/common.proto",
  "modbit/protocol/v1/domain.proto",
  "modbit/protocol/v1/commands.proto",
  "modbit/protocol/v1/events.proto",
].map((f) => join(protoRoot, f));

const pluginBase = join(pkgRoot, "node_modules", ".bin", "protoc-gen-ts_proto");
const plugin = process.platform === "win32" ? `${pluginBase}.cmd` : pluginBase;
if (!existsSync(plugin)) {
  console.error(`ts-proto plugin not found at ${plugin} — run pnpm install`);
  process.exit(2);
}

const args = [
  `--plugin=protoc-gen-ts_proto=${plugin}`,
  `--ts_proto_out=${outDir}`,
  `--proto_path=${protoRoot}`,
  "--ts_proto_opt=esModuleInterop=true,forceLong=string,initializeByDefault=false",
  ...protos,
];

if (!existsSync(outDir)) {
  const { mkdirSync } = await import("node:fs");
  mkdirSync(outDir, { recursive: true });
}

const result = spawnSync("protoc", args, { stdio: "inherit" });
if (result.error || result.status !== 0) {
  console.error("protoc generation failed", result.error ?? `exit ${result.status}`);
  process.exit(1);
}

// Normalize the protoc version comment: it varies between environments
// (brew/apt/choco release trains) and is not meaningful drift. Without this,
// the CI generated-matches-schema gate fails on runner toolchain updates.
const { readdirSync, readFileSync, writeFileSync } = await import("node:fs");
const normalize = (dir) => {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) normalize(p);
    else if (entry.name.endsWith(".ts")) {
      const stable = readFileSync(p, "utf8").replace(
        /^\/\/   protoc\s+v.*$/m,
        "//   protoc               <version-normalized>",
      );
      writeFileSync(p, stable);
    }
  }
};
normalize(outDir);

console.log(`generated TypeScript bindings into ${outDir}`);
