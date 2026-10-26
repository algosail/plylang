// Assemble `@plylang/plylang-<os>-<arch>` from a Cargo release directory.
// The launcher looks for `bin/plylang` and `bin/plylang-ls` inside that package.

import { chmodSync, copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { platformId } from "../bin/native.js";

const root = join(import.meta.dirname, "..");

export function stageNative(platform, binaryDir, outDir, version) {
  const dash = platform.indexOf("-");
  const os = platform.slice(0, dash);
  const cpu = platform.slice(dash + 1);
  if (platformId(os, cpu) !== platform) {
    throw new Error(`unknown platform ${platform}`);
  }
  const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
  const suffix = os === "win32" ? ".exe" : "";
  mkdirSync(join(outDir, "bin"), { recursive: true });
  for (const command of ["plylang", "plylang-ls"]) {
    const name = `${command}${suffix}`;
    const dest = join(outDir, "bin", name);
    copyFileSync(join(binaryDir, name), dest);
    if (suffix === "") {
      chmodSync(dest, 0o755);
    }
  }
  copyFileSync(join(root, "LICENSE"), join(outDir, "LICENSE"));
  writeFileSync(
    join(outDir, "package.json"),
    JSON.stringify(
      {
        name: `@plylang/plylang-${platform}`,
        version,
        description: `plylang binaries for ${platform}`,
        license: "MIT",
        repository: pkg.repository,
        publishConfig: { access: "public" },
        os: [os],
        cpu: [cpu],
        files: ["bin"],
        preferUnplugged: true,
      },
      null,
      2,
    ) + "\n",
  );
}

const invoked = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (invoked) {
  const [platform, binaryDir, outDir] = process.argv.slice(2);
  if (!platform || !binaryDir || !outDir) {
    console.error("usage: node script/stage-native.js <platform> <binary-dir> <out-dir>");
    process.exit(2);
  }
  const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
  stageNative(platform, binaryDir, outDir, pkg.version);
}
