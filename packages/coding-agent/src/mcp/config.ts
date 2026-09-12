/**
 * MCP Configuration Loader for DUM-E
 * Reads MCP server configurations from .dume/mcp.json or user-specified paths.
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { McpConfigFile } from "./types.ts";

export function loadMcpConfig(explicitPath?: string, cwd: string = process.cwd()): McpConfigFile {
	const candidates: string[] = [];

	if (explicitPath) {
		candidates.push(path.resolve(cwd, explicitPath));
	} else {
		candidates.push(
			path.join(cwd, ".dume", "mcp.json"),
			path.join(cwd, ".mcp.json"),
			path.join(os.homedir(), ".dume", "mcp.json"),
		);
	}

	for (const file of candidates) {
		if (fs.existsSync(file)) {
			try {
				const content = fs.readFileSync(file, "utf-8");
				const parsed = JSON.parse(content) as McpConfigFile;
				if (parsed && typeof parsed.mcpServers === "object") {
					return parsed;
				}
			} catch (err) {
				console.warn(`[DUM-E MCP Config] Failed to parse config file ${file}:`, err);
			}
		}
	}

	return { mcpServers: {} };
}
