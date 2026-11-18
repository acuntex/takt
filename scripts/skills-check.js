#!/usr/bin/env node
"use strict";

// Validate the committed OpenCode assets: every skill needs a discoverable description and a
// kebab-case ID, every command needs a description. Broken frontmatter silently hides a skill
// from the model, so this runs in /verify.

const fs = require("fs");
const path = require("path");

const ROOT = path.resolve(__dirname, "..");
const SKILLS = path.join(ROOT, ".opencode", "skills");
const COMMANDS = path.join(ROOT, ".opencode", "commands");
const MAX_DESCRIPTION = 1024;
const MAX_LINES = 250;

const errors = [];
const warnings = [];
let skills = 0;
let commands = 0;

function frontmatter(text) {
  const match = text.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]*)$/);
  if (!match) {
    return { fields: {}, body: text };
  }
  const fields = {};
  let current = null;
  for (const line of match[1].split(/\r?\n/)) {
    const field = line.match(/^([A-Za-z0-9_/-]+):\s*(.*)$/);
    if (field) {
      current = field[1];
      fields[current] = field[2].replace(/^["']|["']$/g, "");
    } else if (current && /^\s+\S/.test(line)) {
      // YAML plain scalars may wrap onto indented continuation lines (Prettier does this).
      fields[current] = (fields[current] + " " + line.trim()).trim();
    }
  }
  return { fields, body: match[2] };
}

function checkFile(file, label) {
  const text = fs.readFileSync(file, "utf8");
  if (text.includes("\t")) {
    errors.push(`${label}: contains tabs (use spaces)`);
  }
  const lines = text.split(/\r?\n/).length;
  if (lines > MAX_LINES) {
    warnings.push(`${label}: ${lines} lines (keep skills under ${MAX_LINES})`);
  }
  return frontmatter(text);
}

function checkSkills() {
  if (!fs.existsSync(SKILLS)) {
    return;
  }
  for (const entry of fs.readdirSync(SKILLS)) {
    const directory = path.join(SKILLS, entry);
    if (!fs.statSync(directory).isDirectory()) {
      continue;
    }
    const file = path.join(directory, "SKILL.md");
    const label = `.opencode/skills/${entry}`;
    if (!fs.existsSync(file)) {
      errors.push(`${label}: missing SKILL.md`);
      continue;
    }
    skills += 1;
    if (!/^[a-z0-9]+(-[a-z0-9]+)*$/.test(entry)) {
      errors.push(`${label}: ID must be lowercase kebab-case`);
    }
    const { fields, body } = checkFile(file, label);
    if (!fields.name) {
      errors.push(`${label}: frontmatter name is missing`);
    }
    if (!fields.description) {
      errors.push(`${label}: frontmatter description is missing (skill is not advertised)`);
    } else {
      if (!fields.description.startsWith("Use when")) {
        errors.push(`${label}: description must start with "Use when"`);
      }
      if (fields.description.length > MAX_DESCRIPTION) {
        errors.push(`${label}: description exceeds ${MAX_DESCRIPTION} characters`);
      }
    }
    if (!body.trim()) {
      errors.push(`${label}: body is empty`);
    }
  }
}

function checkCommands() {
  if (!fs.existsSync(COMMANDS)) {
    return;
  }
  for (const entry of fs.readdirSync(COMMANDS)) {
    if (!entry.endsWith(".md")) {
      continue;
    }
    commands += 1;
    const label = `.opencode/commands/${entry}`;
    const { fields, body } = checkFile(path.join(COMMANDS, entry), label);
    if (!fields.description) {
      errors.push(`${label}: frontmatter description is missing`);
    }
    if (!body.trim()) {
      errors.push(`${label}: body is empty`);
    }
  }
}

checkSkills();
checkCommands();

warnings.forEach((warning) => console.log(`WARN ${warning}`));
if (errors.length) {
  errors.forEach((error) => console.log(`FAIL ${error}`));
  console.log(`\n${errors.length} error(s), ${skills} skills, ${commands} commands`);
  process.exitCode = 1;
} else {
  console.log(`skills-check: ${skills} skills, ${commands} commands OK`);
}
