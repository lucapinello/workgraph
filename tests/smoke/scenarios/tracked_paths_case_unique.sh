#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"

node --input-type=module - "$repo_root" <<'NODE'
import { execFileSync } from "node:child_process";

const root = process.argv[2];
const tracked = execFileSync("git", ["ls-files", "-z"], {
  cwd: root,
  encoding: "utf8",
})
  .split("\0")
  .filter(Boolean);

const byPortableName = new Map();
for (const path of tracked) {
  const key = path.normalize("NFC").toLowerCase();
  const prior = byPortableName.get(key);
  if (prior && prior !== path) {
    console.error(
      `FAIL: tracked paths collide on a case-insensitive filesystem: ${prior} <> ${path}`,
    );
    process.exitCode = 1;
  } else {
    byPortableName.set(key, path);
  }
}

if (!process.exitCode) {
  console.log(`PASS: ${tracked.length} tracked paths have portable case-unique names`);
}
NODE
