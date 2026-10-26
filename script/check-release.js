// The npm package version and the Cargo version stay the same string.
// Its major number is the language version in lib.rs. A GitHub release tag
// is `v` plus that string. Platform packages use the same version.

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { PLATFORMS } from "../bin/native.js";

export function checkRelease(root, tag = process.env.RELEASE_TAG) {
  const errors = [];
  const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
  const cargo = readFileSync(join(root, "Cargo.toml"), "utf8");
  const lib = readFileSync(join(root, "crates/plylang/src/lib.rs"), "utf8");
  const cargoVersion = cargo.match(/^version = "([^"]+)"/m)?.[1];
  const language = lib.match(/LANGUAGE_VERSION:\s*u32\s*=\s*(\d+)/)?.[1];

  if (pkg.name !== "@plylang/plylang") {
    errors.push(`package name is ${pkg.name}, expected @plylang/plylang`);
  }
  if (pkg.private === true) {
    errors.push("package.json is private");
  }
  if (pkg.publishConfig?.access !== "public") {
    errors.push('publishConfig.access must be "public"');
  }
  if (pkg.license !== "MIT") {
    errors.push(`license is ${pkg.license}, expected MIT`);
  }
  if (!pkg.repository?.url?.includes("github.com/algosail/plylang")) {
    errors.push("repository url is missing");
  }
  if (cargoVersion !== pkg.version) {
    errors.push(`Cargo version ${cargoVersion} differs from package ${pkg.version}`);
  }
  const [major] = String(pkg.version).split(".");
  if (major !== language) {
    errors.push(`package ${pkg.version} does not implement language ${language}`);
  }
  const expected = [...PLATFORMS].map((id) => `@plylang/plylang-${id}`).sort();
  const present = Object.keys(pkg.optionalDependencies ?? {}).sort();
  if (present.join("\n") !== expected.join("\n")) {
    errors.push(`optional packages:\n${present.join("\n")}\nexpected:\n${expected.join("\n")}`);
  }
  for (const [name, version] of Object.entries(pkg.optionalDependencies ?? {})) {
    if (version !== pkg.version) {
      errors.push(`${name} is ${version}, expected ${pkg.version}`);
    }
  }
  for (const file of pkg.files ?? []) {
    try {
      readFileSync(join(root, file));
    } catch {
      errors.push(`files entry is missing: ${file}`);
    }
  }
  for (const [command, file] of Object.entries(pkg.bin ?? {})) {
    if (file !== `bin/${command}.js`) {
      errors.push(`bin ${command} points at ${file}`);
    }
  }
  if (tag !== undefined && tag !== "") {
    if (tag !== `v${pkg.version}`) {
      errors.push(`release tag ${tag} is not v${pkg.version}`);
    }
  }
  return errors;
}

const invoked = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (invoked) {
  const errors = checkRelease(join(import.meta.dirname, ".."));
  if (errors.length > 0) {
    for (const error of errors) {
      console.error(error);
    }
    process.exit(1);
  }
}
