#!/usr/bin/env node
// Resolve starts here, in this package, so Node sees the server's optional
// platform package. The shared lookup lives with the compiler.

import { createRequire } from "node:module";
import { run } from "@plylang/plylang/bin/native.js";

const require = createRequire(import.meta.url);

run("plylang-ls", {
  resolvePackage(name) {
    try {
      return require.resolve(`${name}/package.json`);
    } catch {
      return null;
    }
  },
});
