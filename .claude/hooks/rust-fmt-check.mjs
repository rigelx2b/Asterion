// PostToolUse hook: after Claude edits a .rs file, run `cargo fmt --all -- --check`
// in the nearest Cargo workspace. Check-only: it never rewrites files, never uses
// the network, and never blocks Claude. A failed check exits 1, which Claude Code
// shows to the user as a non-blocking warning.
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

let input = {};
try {
  input = JSON.parse(readFileSync(0, "utf8") || "{}");
} catch {
  process.exit(0);
}

const filePath = input?.tool_input?.file_path;
if (typeof filePath !== "string" || !filePath.endsWith(".rs")) process.exit(0);

const projectDir = resolve(process.env.CLAUDE_PROJECT_DIR || input.cwd || process.cwd());

// Walk up from the edited file to find Cargo.toml, staying inside the project.
let dir = dirname(resolve(filePath));
let crateDir = null;
while (dir.toLowerCase().startsWith(projectDir.toLowerCase())) {
  if (existsSync(join(dir, "Cargo.toml"))) crateDir = dir;
  const parent = dirname(dir);
  if (parent === dir) break;
  dir = parent;
}
if (!crateDir) process.exit(0);

const result = spawnSync("cargo", ["fmt", "--all", "--", "--check"], {
  cwd: crateDir,
  encoding: "utf8",
  timeout: 50_000,
});

if (result.status === 0) process.exit(0);

const detail = (result.stdout || result.stderr || String(result.error || "")).trim();
const lines = detail.split(/\r?\n/).slice(0, 20).join("\n");
process.stderr.write(
  `[rust-fmt-check] cargo fmt --check reported differences in ${crateDir}.\n` +
    `Run \`cargo fmt --all\` manually to apply formatting.\n${lines}\n`,
);
process.exit(1);
