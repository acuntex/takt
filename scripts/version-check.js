#!/usr/bin/env node
"use strict";

// The package version has one source: packaging/appinfo.json. Four other files repeat it, and a
// stale copy is silent — the Channel serves one version while the package says another, and the
// --locked CI jobs fail on a lock file that disagrees. This check turns every disagreement into a
// non-zero exit before a release can be built.
//
// Usage: node scripts/version-check.js [--appinfo <path>] [--cargo <path>]

const fs = require("fs");
const path = require("path");

const ROOT = path.resolve(__dirname, "..");

function argument(name, fallback) {
  const index = process.argv.indexOf(name);
  if (index === -1) {
    return fallback;
  }
  const value = process.argv[index + 1];
  if (!value || value.startsWith("--")) {
    console.error(`${name} needs a path`);
    process.exit(2);
  }
  return value;
}

const appinfoPath = argument("--appinfo", path.join(ROOT, "packaging", "appinfo.json"));

function read(file) {
  return fs.readFileSync(file, "utf8");
}

function jsonVersion(file) {
  return JSON.parse(read(file)).version;
}

function firstMatch(file, pattern) {
  const match = read(file).match(pattern);
  return match ? match[1] : null;
}

function lockVersion(file) {
  const lines = read(file).split(/\r?\n/);
  for (let index = 0; index < lines.length; index += 1) {
    if (lines[index].trim() !== 'name = "takt"') {
      continue;
    }
    for (let next = index + 1; next < lines.length; next += 1) {
      if (lines[next].trim() === "") {
        break;
      }
      const version = lines[next].match(/^version\s*=\s*"([^"]+)"/);
      if (version) {
        return version[1];
      }
    }
  }
  return null;
}

const reference = jsonVersion(appinfoPath);
const sources = [
  [
    "Cargo",
    firstMatch(
      argument("--cargo", path.join(ROOT, "daemon", "Cargo.toml")),
      /^version\s*=\s*"([^"]+)"/m
    ),
  ],
  ["lock", lockVersion(path.join(ROOT, "daemon", "Cargo.lock"))],
  ["setup.sh", firstMatch(path.join(ROOT, "packaging", "setup.sh"), /"version":\s*"([^"]+)"/)],
  [
    "ls2",
    jsonVersion(path.join(ROOT, "packaging", "ls2", "manifests.d", "com.github.acuntex.takt.json")),
  ],
];

const mismatches = sources.filter(([, version]) => version !== reference);
if (mismatches.length > 0) {
  for (const [label, version] of mismatches) {
    console.error(
      `version-check: ${label} is ${version || "missing"}, appinfo.json says ${reference}`
    );
  }
  console.error("version-check: bump every copy in the same commit");
  process.exit(1);
}

const summary = sources.map(([label, version]) => `${label} ${version}`).join(" / ");
console.log(`version-check: appinfo ${reference} / ${summary} OK`);
