/**
 * MCP Tool Bridge for DUM-E
 * Bridges discovered MCP tools into native @dum-e/agent-core AgentTool instances.
 */

import type { AgentTool, AgentToolResult } from "@dum-e/agent-core";
import type { ImageContent, TextContent } from "@dum-e/ai";
import { Type } from "typebox";
import type { McpClient } from "./client.ts";
import type { McpToolDefinition } from "./types.ts";

export function bridgeMcpTool(client: McpClient, mcpTool: McpToolDefinition): AgentTool {
	// Namespace the tool name to avoid collisions: mcp__<server>__<tool>
	const wireName = `mcp__${client.serverName}__${mcpTool.name}`;
	const description = mcpTool.description ?? `MCP Tool ${mcpTool.name} provided by server ${client.serverName}`;

	const schema =
		mcpTool.inputSchema && typeof mcpTool.inputSchema === "object"
			? Type.Unsafe(mcpTool.inputSchema)
			: Type.Object({});

	return {
		name: wireName,
		label: `${client.serverName}: ${mcpTool.name}`,
		description,
		parameters: schema,
		execute: async (_callId: string, params: any, signal?: AbortSignal): Promise<AgentToolResult<unknown>> => {
			try {
				const result = await client.callTool(mcpTool.name, params, signal);
				const content: (TextContent | ImageContent)[] = [];

				if (Array.isArray(result.content)) {
					for (const item of result.content) {
						if (item.type === "text" && typeof item.text === "string") {
							content.push({ type: "text", text: item.text });
						} else if (
							item.type === "image" &&
							typeof item.data === "string" &&
							typeof item.mimeType === "string"
						) {
							content.push({
								type: "image",
								data: item.data,
								mimeType: item.mimeType,
							});
						} else if (item.type === "resource" && item.resource) {
							const textContent =
								item.resource.text ??
								(item.resource.blob ? `[Binary Resource: ${item.resource.uri}]` : item.resource.uri);
							content.push({
								type: "text",
								text: `Resource (${item.resource.uri}):\n${textContent}`,
							});
						}
					}
				}

				if (content.length === 0) {
					content.push({ type: "text", text: "(Empty result from MCP tool)" });
				}

				return {
					content,
					details: {
						server: client.serverName,
						tool: mcpTool.name,
						isError: Boolean(result.isError),
					},
				};
			} catch (err: any) {
				return {
					content: [
						{
							type: "text",
							text: `MCP Tool Execution Failed: ${err.message || String(err)}`,
						},
					],
					details: {
						server: client.serverName,
						tool: mcpTool.name,
						isError: true,
						error: err.message,
					},
				};
			}
		},
	};
}
