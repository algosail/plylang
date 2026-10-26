// The npm commands `plylang` and `plylang-ls` start here and run the Rust
// binary. An installed optional package `@plylang/plylang-<os>-<arch>` wins.
// A checkout of this repository uses `target/release`, then `target/debug`.

import { spawnSync } from "node:child_process";
import { accessSync, constants, statSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";

const root = join(import.meta.dirname, "..");
const require = createRequire(import.meta.url);

export const PLATFORMS = new Set([
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64",
  "linux-x64",
  "win32-arm64",
  "win32-x64",
]);

export function platformId(platform = process.platform, arch = process.arch) {
  const id = `${platform}-${arch}`;
  return PLATFORMS.has(id) ? id : null;
}

export function resolveBinary(command, options = {}) {
  if (command !== "plylang" && command !== "plylang-ls") {
    throw new Error(`unknown command ${command}`);
  }
  const env = options.env ?? process.env;
  const platform = options.platform ?? process.platform;
  const arch = options.arch ?? process.arch;
  const packageRoot = options.packageRoot ?? root;
  const resolvePackage = options.resolvePackage ?? defaultResolvePackage;
  const overrideName = command === "plylang" ? "PLYLANG_BIN" : "PLYLANG_LS_BIN";
  const override = env[overrideName];
  if (override) {
    if (!isExecutable(override)) {
      throw new Error(`${overrideName} is not an executable file: ${override}`);
    }
    return override;
  }

  const id = platformId(platform, arch);
  if (id) {
    const pkgName = `@plylang/plylang-${id}`;
    const pkgJson = resolvePackage(pkgName);
    if (pkgJson) {
      const name = fileName(command, platform);
      const file = join(dirname(pkgJson), "bin", name);
      if (!isExecutable(file)) {
        throw new Error(`${pkgName} does not contain an executable bin/${name}`);
      }
      return file;
    }
  }

  const built = checkoutBinary(command, packageRoot, platform);
  if (built) {
    return built;
  }
  if (!id) {
    throw new Error(`${command} does not ship a binary for ${platform}-${arch}.`);
  }
  return missing(command, id);
}

function missing(command, id) {
  throw new Error(
    `${command} was not found for ${id}.\n` +
      "From a checkout, run `cargo build --release -p plylang -p plylang-ls`.\n" +
      `From npm, install the optional package @plylang/plylang-${id}.`,
  );
}

function checkoutBinary(command, packageRoot, platform) {
  const name = fileName(command, platform);
  for (const profile of ["release", "debug"]) {
    const file = join(packageRoot, "target", profile, name);
    if (isExecutable(file)) {
      return file;
    }
  }
  return null;
}

function fileName(command, platform) {
  return platform === "win32" ? `${command}.exe` : command;
}

function defaultResolvePackage(name) {
  try {
    return require.resolve(`${name}/package.json`);
  } catch {
    return null;
  }
}

export function run(command) {
  let bin;
  try {
    bin = resolveBinary(command);
  } catch (error) {
    console.error(error.message);
    process.exit(1);
  }
  const child = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
  if (child.error) {
    console.error(`${command}: ${child.error.message}`);
    process.exit(1);
  }
  process.exit(child.status ?? 1);
}

function isExecutable(file) {
  try {
    if (!statSync(file).isFile()) {
      return false;
    }
    if (process.platform !== "win32") {
      accessSync(file, constants.X_OK);
    }
    return true;
  } catch {
    return false;
  }
}
