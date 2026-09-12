#!/usr/bin/env node
/**
 * Local Fixture MCP Server for DUM-E MCP integration tests.
 * Communicates via newline-delimited JSON-RPC on stdin/stdout.
 */

import readline from "node:readline";

const rl = readline.createInterface({
	input: process.stdin,
	output: process.stdout,
	terminal: false,
});

function send(msg) {
	process.stdout.write(JSON.stringify(msg) + "\n");
}

rl.on("line", (line) => {
	const trimmed = line.trim();
	if (!trimmed) return;

	let msg;
	try {
		msg = JSON.parse(trimmed);
	} catch {
		return;
	}

	if (msg.method === "initialize") {
		send({
			jsonrpc: "2.0",
			id: msg.id,
			result: {
				protocolVersion: "2024-11-05",
				capabilities: { tools: {} },
				serverInfo: { name: "fixture-mcp", version: "1.0.0" },
			},
		});
	} else if (msg.method === "notifications/initialized") {
		// Handshake complete
	} else if (msg.method === "tools/list") {
		send({
			jsonrpc: "2.0",
			id: msg.id,
			result: {
				tools: [
					{
						name: "echo_message",
						description: "Echoes back the message",
						inputSchema: {
							type: "object",
							properties: {
								message: { type: "string" },
							},
							required: ["message"],
						},
					},
					{
						name: "fail_operation",
						description: "Fails intentionally",
						inputSchema: { type: "object", properties: {} },
					},
					{
						name: "slow_operation",
						description: "Delays response to test abort/cancellation",
						inputSchema: { type: "object", properties: {} },
					},
				],
			},
		});
	} else if (msg.method === "tools/call") {
		const { name, arguments: args } = msg.params || {};
		if (name === "echo_message") {
			send({
				jsonrpc: "2.0",
				id: msg.id,
				result: {
					content: [{ type: "text", text: `Echo: ${args?.message}` }],
					isError: false,
				},
			});
		} else if (name === "fail_operation") {
			send({
				jsonrpc: "2.0",
				id: msg.id,
				result: {
					content: [{ type: "text", text: "Operation failed intentionally" }],
					isError: true,
				},
			});
		} else if (name === "slow_operation") {
			// Do not reply immediately; will test client cancellation
			setTimeout(() => {
				send({
					jsonrpc: "2.0",
					id: msg.id,
					result: {
						content: [{ type: "text", text: "Finished slow operation" }],
					},
				});
			}, 3000);
		} else {
			send({
				jsonrpc: "2.0",
				id: msg.id,
				error: { code: -32601, message: `Tool not found: ${name}` },
			});
		}
	} else if (msg.method === "notifications/cancelled") {
		// Received cancellation notification
	}
});
