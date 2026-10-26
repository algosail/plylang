import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { platformId, resolveBinary } from "./native.js";

const root = join(import.meta.dirname, "..");

test("names a supported platform", () => {
  assert.equal(platformId("darwin", "arm64"), "darwin-arm64");
  assert.equal(platformId("linux", "x64"), "linux-x64");
  assert.equal(platformId("win32", "arm64"), "win32-arm64");
  assert.equal(platformId("sunos", "x64"), null);
});

test("uses the checkout binary when no platform package is installed", () => {
  const dir = mkdtempSync(join(tmpdir(), "plylang-checkout-"));
  const release = join(dir, "target", "release");
  mkdirSync(release, { recursive: true });
  const file = join(release, process.platform === "win32" ? "plylang.exe" : "plylang");
  writeFileSync(file, "#!/bin/sh\n");
  chmodSync(file, 0o755);
  const found = resolveBinary("plylang", {
    env: {},
    resolvePackage: () => null,
    packageRoot: dir,
  });
  assert.equal(found, file);
});

test("finds the language server the same way", () => {
  const dir = mkdtempSync(join(tmpdir(), "plylang-checkout-"));
  const debug = join(dir, "target", "debug");
  mkdirSync(debug, { recursive: true });
  const file = join(debug, process.platform === "win32" ? "plylang-ls.exe" : "plylang-ls");
  writeFileSync(file, "#!/bin/sh\n");
  chmodSync(file, 0o755);
  const found = resolveBinary("plylang-ls", {
    env: {},
    resolvePackage: () => null,
    packageRoot: dir,
  });
  assert.equal(found, file);
});

test("an override replaces the checkout binary", () => {
  const dir = mkdtempSync(join(tmpdir(), "plylang-bin-"));
  const file = join(dir, "plylang");
  writeFileSync(file, "#!/bin/sh\nprintf 'override\\n'\n");
  chmodSync(file, 0o755);
  assert.equal(resolveBinary("plylang", { env: { PLYLANG_BIN: file } }), file);
  const ran = spawnSync(process.execPath, ["bin/plylang.js"], {
    cwd: root,
    env: { ...process.env, PLYLANG_BIN: file },
    encoding: "utf8",
  });
  assert.equal(ran.status, 0);
  assert.equal(ran.stdout, "override\n");
});

test("a missing override is an error", () => {
  assert.throws(
    () => resolveBinary("plylang", { env: { PLYLANG_BIN: "/no/such/plylang" } }),
    /PLYLANG_BIN is not an executable file/,
  );
});

test("a platform package wins over the checkout", () => {
  const dir = mkdtempSync(join(tmpdir(), "plylang-pkg-"));
  mkdirSync(join(dir, "bin"));
  const file = join(dir, "bin", "plylang");
  writeFileSync(file, "#!/bin/sh\n");
  chmodSync(file, 0o755);
  writeFileSync(join(dir, "package.json"), "{}\n");
  const found = resolveBinary("plylang", {
    env: {},
    platform: "darwin",
    arch: "arm64",
    resolvePackage(name) {
      assert.equal(name, "@plylang/plylang-darwin-arm64");
      return join(dir, "package.json");
    },
  });
  assert.equal(found, file);
});

test("a platform package without the binary is an error", () => {
  const dir = mkdtempSync(join(tmpdir(), "plylang-empty-"));
  writeFileSync(join(dir, "package.json"), "{}\n");
  assert.throws(
    () =>
      resolveBinary("plylang-ls", {
        env: {},
        platform: "linux",
        arch: "x64",
        resolvePackage: () => join(dir, "package.json"),
      }),
    /does not contain an executable bin\/plylang-ls/,
  );
});

test("an unknown platform without a checkout binary is refused", () => {
  assert.throws(
    () =>
      resolveBinary("plylang", {
        env: {},
        platform: "sunos",
        arch: "x64",
        packageRoot: "/no/such/checkout",
        resolvePackage: () => null,
      }),
    /does not ship a binary for sunos-x64/,
  );
});

test("plylang with no arguments prints usage", () => {
  const ran = spawnSync(process.execPath, ["bin/plylang.js"], {
    cwd: root,
    encoding: "utf8",
  });
  assert.equal(ran.status, 2);
  assert.match(ran.stderr, /usage: plylang check \[dir\]/);
});
