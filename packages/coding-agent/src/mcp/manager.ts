/**
 * MCP Manager for DUM-E
 * Coordinates server discovery, connection pooling, tool registration, and lifecycle.
 */

import type { AgentTool } from "@dum-e/agent-core";
import { McpClient } from "./client.ts";
import { loadMcpConfig } from "./config.ts";
import { bridgeMcpTool } from "./tool-bridge.ts";
import { McpStdioTransport } from "./transport-stdio.ts";
import type { McpServerConfig, McpStdioServerConfig } from "./types.ts";

export class McpManager {
	private clients = new Map<string, McpClient>();
	private serverConfigs = new Map<string, McpServerConfig>();
	private registeredTools: AgentTool[] = [];

	async loadFromConfig(configPath?: string, cwd: string = process.cwd()): Promise<void> {
		const config = loadMcpConfig(configPath, cwd);
		if (config.mcpServers) {
			await this.registerServers(config.mcpServers);
		}
	}

	async registerServers(servers: Record<string, McpServerConfig>): Promise<void> {
		for (const [name, cfg] of Object.entries(servers)) {
			this.serverConfigs.set(name, cfg);
			try {
				await this.connectServer(name, cfg);
			} catch (err) {
				console.warn(`[DUM-E MCP Manager] Failed to connect to MCP server "${name}":`, err);
			}
		}
	}

	async connectServer(name: string, cfg: McpServerConfig): Promise<McpClient> {
		if ("command" in cfg) {
			const transport = new McpStdioTransport(cfg as McpStdioServerConfig);
			const client = new McpClient({
				serverName: name,
				transport,
			});
			await client.connect();
			this.clients.set(name, client);

			// Discover and bridge tools
			const discovered = await client.listTools();
			for (const toolDef of discovered) {
				const agentTool = bridgeMcpTool(client, toolDef);
				this.registeredTools.push(agentTool);
			}

			return client;
		}

		throw new Error(`Unsupported MCP server transport configuration for "${name}"`);
	}

	async reconnectServer(name: string): Promise<McpClient | null> {
		const cfg = this.serverConfigs.get(name);
		if (!cfg) return null;

		const existing = this.clients.get(name);
		if (existing) {
			await existing.close().catch(() => {});
			this.clients.delete(name);
		}

		// Remove old bridged tools for this server
		this.registeredTools = this.registeredTools.filter((t) => !t.name.startsWith(`mcp__${name}__`));

		return this.connectServer(name, cfg);
	}

	getTools(): AgentTool[] {
		return [...this.registeredTools];
	}

	getClient(serverName: string): McpClient | undefined {
		return this.clients.get(serverName);
	}

	async close(): Promise<void> {
		for (const client of this.clients.values()) {
			await client.close().catch(() => {});
		}
		this.clients.clear();
		this.registeredTools = [];
	}
}
