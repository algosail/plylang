// The compiler package, the language-server package, and Cargo share one version
// string. Its major number is the language version in lib.rs. A GitHub release
// tag is `v` plus that string. Each platform package uses the same version.

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { PLATFORMS, npmPackage } from "../bin/native.js";

export function checkRelease(root, tag = process.env.RELEASE_TAG) {
  const errors = [];
  const pkg = readJson(root, "package.json", errors);
  const server = readJson(root, "server/package.json", errors);
  const cargo = readFileSync(join(root, "Cargo.toml"), "utf8");
  const lib = readFileSync(join(root, "crates/plylang/src/lib.rs"), "utf8");
  const cargoVersion = cargo.match(/^version = "([^"]+)"/m)?.[1];
  const language = lib.match(/LANGUAGE_VERSION:\s*u32\s*=\s*(\d+)/)?.[1];

  if (pkg) {
    checkPackage(root, pkg, errors, {
      name: npmPackage("plylang"),
      command: "plylang",
      platformPrefix: `${npmPackage("plylang")}-`,
    });
    if (cargoVersion !== pkg.version) {
      errors.push(`Cargo version ${cargoVersion} differs from package ${pkg.version}`);
    }
    const [major] = String(pkg.version).split(".");
    if (major !== language) {
      errors.push(`package ${pkg.version} does not implement language ${language}`);
    }
    if (tag !== undefined && tag !== "" && tag !== `v${pkg.version}`) {
      errors.push(`release tag ${tag} is not v${pkg.version}`);
    }
  }
  if (server) {
    checkPackage(join(root, "server"), server, errors, {
      name: npmPackage("plylang-ls"),
      command: "plylang-ls",
      platformPrefix: `${npmPackage("plylang-ls")}-`,
    });
    if (pkg && server.version !== pkg.version) {
      errors.push(`server package ${server.version} differs from compiler ${pkg.version}`);
    }
    const dep = server.dependencies?.["@plylang/plylang"];
    if (pkg && dep !== pkg.version) {
      errors.push(`server depends on @plylang/plylang ${dep}, expected ${pkg.version}`);
    }
  }
  return errors;
}

function readJson(root, rel, errors) {
  try {
    return JSON.parse(readFileSync(join(root, rel), "utf8"));
  } catch {
    errors.push(`cannot read ${rel}`);
    return null;
  }
}

function checkPackage(dir, pkg, errors, spec) {
  if (pkg.name !== spec.name) {
    errors.push(`package name is ${pkg.name}, expected ${spec.name}`);
  }
  if (pkg.private === true) {
    errors.push(`${spec.name} package.json is private`);
  }
  if (pkg.publishConfig?.access !== "public") {
    errors.push(`${spec.name} publishConfig.access must be "public"`);
  }
  if (pkg.license !== "MIT") {
    errors.push(`${spec.name} license is ${pkg.license}, expected MIT`);
  }
  if (!pkg.repository?.url?.includes("github.com/algosail/plylang")) {
    errors.push(`${spec.name} repository url is missing`);
  }
  const expected = [...PLATFORMS].map((id) => `${spec.platformPrefix}${id}`).sort();
  const present = Object.keys(pkg.optionalDependencies ?? {}).sort();
  if (present.join("\n") !== expected.join("\n")) {
    errors.push(
      `${spec.name} optional packages:\n${present.join("\n")}\nexpected:\n${expected.join("\n")}`,
    );
  }
  for (const [name, version] of Object.entries(pkg.optionalDependencies ?? {})) {
    if (version !== pkg.version) {
      errors.push(`${name} is ${version}, expected ${pkg.version}`);
    }
  }
  for (const file of pkg.files ?? []) {
    try {
      readFileSync(join(dir, file));
    } catch {
      errors.push(`${spec.name} files entry is missing: ${file}`);
    }
  }
  const bins = Object.entries(pkg.bin ?? {});
  if (bins.length !== 1 || bins[0][0] !== spec.command || bins[0][1] !== `bin/${spec.command}.js`) {
    errors.push(`${spec.name} bin is ${JSON.stringify(pkg.bin)}`);
  }
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
