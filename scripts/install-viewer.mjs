// postinstall: installs the viewer's dependencies (viewer/ is a separate npm project).
//
// Runs npm with viewer/ as the working directory and without the parent install's npm_*
// environment. `npm install --prefix viewer` from a lifecycle script re-installed the root
// project on Windows runners and recursed until the machine ran out of processes.
// `npm ci` at the root uses `npm ci` for the viewer too, so CI installs stay locked.

import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const viewer = join(dirname(fileURLToPath(import.meta.url)), "..", "viewer");
const command = process.env.npm_command === "ci" ? "ci" : "install";
const npmCli = process.env.npm_execpath;

const env = Object.fromEntries(
    Object.entries(process.env).filter(([key]) => !key.toLowerCase().startsWith("npm_")),
);

// Prefer the npm that is running this script; fall back to whatever `npm` is on PATH.
const result = npmCli
    ? spawnSync(process.execPath, [npmCli, command], { cwd: viewer, env, stdio: "inherit" })
    : spawnSync("npm", [command], { cwd: viewer, env, stdio: "inherit", shell: true });

if (result.error) throw result.error;
process.exit(result.status ?? 1);
