/**
 * MCP Client for DUM-E
 * Handles protocol initialization, tool discovery, and tool execution over MCP transports.
 */

import type { McpToolCallResult, McpToolDefinition, McpTransport } from "./types.ts";

export interface McpClientOptions {
	serverName: string;
	transport: McpTransport;
}

export class McpClient {
	public readonly serverName: string;
	private transport: McpTransport;
	private initialized = false;

	constructor(options: McpClientOptions) {
		this.serverName = options.serverName;
		this.transport = options.transport;
	}

	async connect(): Promise<void> {
		if (this.initialized) return;

		await this.transport.connect();

		// MCP Protocol Handshake
		await this.transport.sendRequest("initialize", {
			protocolVersion: "2024-11-05",
			clientInfo: {
				name: "dum-e",
				version: "0.0.3",
			},
			capabilities: {
				roots: { listChanged: true },
				sampling: {},
			},
		});

		// Confirm initialization
		await this.transport.sendNotification("notifications/initialized");
		this.initialized = true;
	}

	async listTools(): Promise<McpToolDefinition[]> {
		if (!this.initialized) {
			await this.connect();
		}

		const result = (await this.transport.sendRequest("tools/list", {})) as {
			tools?: McpToolDefinition[];
		};

		return result?.tools ?? [];
	}

	async callTool(name: string, args: Record<string, unknown> = {}, signal?: AbortSignal): Promise<McpToolCallResult> {
		if (!this.initialized) {
			await this.connect();
		}

		const result = (await this.transport.sendRequest(
			"tools/call",
			{
				name,
				arguments: args,
			},
			signal,
		)) as McpToolCallResult;

		return result;
	}

	async close(): Promise<void> {
		this.initialized = false;
		await this.transport.close();
	}
}
