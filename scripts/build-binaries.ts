#!/usr/bin/env bun
/**
 * DUM-E Cross-Platform Binary Builder
 * Uses Bun compile to build standalone executables for all supported targets.
 */
import { spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";

const projectRoot = path.resolve(import.meta.dir, "..");
const outDir = path.join(projectRoot, "dist-bin");
const entryPoint = path.join(projectRoot, "packages", "coding-agent", "src", "cli.ts");

if (!fs.existsSync(outDir)) {
	fs.mkdirSync(outDir, { recursive: true });
}

// Current host build
console.log("==> Compiling host DUM-E binary...");
const hostTarget = `${process.platform}-${process.arch === "arm64" ? "arm64" : "x64"}`;
const hostOut = path.join(outDir, `dume-${hostTarget}`);

const res = spawnSync(
	"bun",
	["build", "--compile", "--minify", entryPoint, "--outfile", hostOut],
	{ cwd: projectRoot, stdio: "inherit" }
);

if (res.status !== 0) {
	console.error("Host build failed!");
	process.exit(1);
}

console.log(`==> Built host binary: ${hostOut}`);

// Create a generic 'dume' symlink or copy for convenience
fs.copyFileSync(hostOut, path.join(outDir, "dume"));
fs.chmodSync(path.join(outDir, "dume"), 0o755);

console.log("==> All builds complete in dist-bin/");
