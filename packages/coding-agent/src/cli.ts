#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { setupCli } from "./cli/setup.ts";
import { main } from "./main.ts";

const repoRoot = resolve(import.meta.dirname, "../../../");
const rustBinaryRelease = resolve(repoRoot, "target/release/dume");
const rustBinaryDebug = resolve(repoRoot, "target/debug/dume");
const rustBinary = existsSync(rustBinaryRelease)
	? rustBinaryRelease
	: existsSync(rustBinaryDebug)
		? rustBinaryDebug
		: null;

if (rustBinary && process.env.DUME_LEGACY_TS !== "1") {
	const res = spawnSync(rustBinary, process.argv.slice(2), {
		stdio: "inherit",
		env: process.env,
	});
	process.exit(res.status ?? 0);
}

setupCli();
main(process.argv.slice(2));
