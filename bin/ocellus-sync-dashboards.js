#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const tsxPackageJson = require.resolve("tsx/package.json");
const tsxCli = join(dirname(tsxPackageJson), "dist", "cli.mjs");
const script = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "site",
  "scripts",
  "sync-grafana-dashboards.ts",
);

const result = spawnSync(process.execPath, [tsxCli, script, ...process.argv.slice(2)], {
  stdio: "inherit",
});

if (result.error) {
  throw result.error;
}

process.exit(result.status ?? 1);
