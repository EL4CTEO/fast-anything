#!/usr/bin/env node
// Runs the prebuilt fast-anything binary for this platform (installed as an optional dependency).
"use strict";
const { spawnSync } = require("node:child_process");
const { chmodSync } = require("node:fs");

const pkg = `@el4cteo/fast-anything-${process.platform}-${process.arch}`;
const exe = process.platform === "win32" ? "fast-anything.exe" : "fast-anything";
let bin;
try {
  bin = require.resolve(`${pkg}/bin/${exe}`);
} catch {
  console.error(
    `fast-anything: no prebuilt binary for ${process.platform}-${process.arch} (${pkg} isn't installed).\n` +
      "Reinstall without --omit=optional, or build it: cargo install fast-anything",
  );
  process.exit(1);
}
// A package published without the execute bit would otherwise fail with EACCES.
if (process.platform !== "win32") {
  try {
    chmodSync(bin, 0o755);
  } catch {}
}
const run = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (run.error) {
  console.error(`fast-anything: ${run.error.message}`);
  process.exit(1);
}
process.exit(run.status ?? 1);
