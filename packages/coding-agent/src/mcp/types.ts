/**
 * MCP (Model Context Protocol) Type Definitions for DUM-E
 * Conforms to MCP Specification (2024-11-05 / 2025-03-26)
 */

export interface JsonRpcRequest {
	jsonrpc: "2.0";
	id: string | number;
	method: string;
	params?: unknown;
}

export interface JsonRpcResponse {
	jsonrpc: "2.0";
	id: string | number;
	result?: unknown;
	error?: {
		code: number;
		message: string;
		data?: unknown;
	};
}

export interface JsonRpcNotification {
	jsonrpc: "2.0";
	method: string;
	params?: unknown;
}

export type JsonRpcMessage = JsonRpcRequest | JsonRpcResponse | JsonRpcNotification;

export interface McpStdioServerConfig {
	command: string;
	args?: string[];
	env?: Record<string, string>;
	cwd?: string;
}

export interface McpHttpServerConfig {
	url: string;
	headers?: Record<string, string>;
}

export type McpServerConfig = McpStdioServerConfig | McpHttpServerConfig;

export interface McpConfigFile {
	mcpServers?: Record<string, McpServerConfig>;
}

export interface McpToolInputSchema {
	type: "object";
	properties?: Record<string, unknown>;
	required?: string[];
	[key: string]: unknown;
}

export interface McpToolDefinition {
	name: string;
	description?: string;
	inputSchema: McpToolInputSchema;
}

export interface McpContentText {
	type: "text";
	text: string;
}

export interface McpContentImage {
	type: "image";
	data: string;
	mimeType: string;
}

export interface McpContentResource {
	type: "resource";
	resource: {
		uri: string;
		mimeType?: string;
		text?: string;
		blob?: string;
	};
}

export type McpContent = McpContentText | McpContentImage | McpContentResource;

export interface McpToolCallResult {
	content: McpContent[];
	isError?: boolean;
}

export interface McpTransport {
	connect(): Promise<void>;
	sendRequest(method: string, params?: unknown, signal?: AbortSignal): Promise<unknown>;
	sendNotification(method: string, params?: unknown): Promise<void>;
	close(): Promise<void>;
}
