import * as path from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { createAgentSession } from "../src/core/sdk.ts";
import { SessionManager } from "../src/core/session-manager.ts";
import { bridgeMcpTool, McpClient, McpManager, McpStdioTransport } from "../src/mcp/index.ts";

describe("DUM-E MCP Runtime Subsystem", () => {
	const fixtureServerPath = path.resolve(__dirname, "fixtures/mcp-fixture-server.mjs");
	let transport: McpStdioTransport;
	let client: McpClient;

	beforeEach(async () => {
		transport = new McpStdioTransport({
			command: process.execPath,
			args: [fixtureServerPath],
		});
		client = new McpClient({
			serverName: "test-fixture",
			transport,
		});
		await client.connect();
	});

	afterEach(async () => {
		await client.close();
	});

	test("MCP Handshake and Tool Discovery", async () => {
		const tools = await client.listTools();
		expect(tools.length).toBe(3);

		const echo = tools.find((t) => t.name === "echo_message");
		expect(echo).toBeDefined();
		expect(echo?.description).toBe("Echoes back the message");
		expect(echo?.inputSchema.properties).toHaveProperty("message");
	});

	test("MCP Tool Execution and Content Conversion", async () => {
		const res = await client.callTool("echo_message", { message: "Hello DUM-E" });
		expect(res.isError).toBe(false);
		expect(res.content).toEqual([{ type: "text", text: "Echo: Hello DUM-E" }]);
	});

	test("MCP Tool Error Handling", async () => {
		const res = await client.callTool("fail_operation", {});
		expect(res.isError).toBe(true);
		expect((res.content[0] as any).text).toContain("failed intentionally");
	});

	test("MCP Request Abort and Cancellation", async () => {
		const abortController = new AbortController();
		const callPromise = client.callTool("slow_operation", {}, abortController.signal);

		setTimeout(() => {
			abortController.abort();
		}, 50);

		await expect(callPromise).rejects.toThrow("was aborted");
	});

	test("MCP Tool Bridge to AgentTool Interface", async () => {
		const tools = await client.listTools();
		const echoDef = tools.find((t) => t.name === "echo_message")!;

		const agentTool = bridgeMcpTool(client, echoDef);
		expect(agentTool.name).toBe("mcp__test-fixture__echo_message");
		expect(agentTool.description).toBe(echoDef.description);

		const toolResult = await agentTool.execute("call-123", { message: "Bridged execution" });
		expect(toolResult.content[0]).toEqual({
			type: "text",
			text: "Echo: Bridged execution",
		});
		expect(toolResult.details).toEqual({
			server: "test-fixture",
			tool: "echo_message",
			isError: false,
		});
	});

	test("MCP Manager: Multi-Server Namespacing, Reconnection and Tool Aggregation", async () => {
		const manager = new McpManager();

		await manager.registerServers({
			srv1: {
				command: process.execPath,
				args: [fixtureServerPath],
			},
			srv2: {
				command: process.execPath,
				args: [fixtureServerPath],
			},
		});

		const tools = manager.getTools();
		expect(tools.length).toBe(6); // 3 from srv1, 3 from srv2

		// Verify namespacing prevents collision
		const names = tools.map((t) => t.name);
		expect(names).toContain("mcp__srv1__echo_message");
		expect(names).toContain("mcp__srv2__echo_message");

		// Test server reconnection
		const reconnected = await manager.reconnectServer("srv1");
		expect(reconnected).not.toBeNull();

		const refreshedTools = manager.getTools();
		expect(refreshedTools.length).toBe(6);

		await manager.close();
	});

	test("MCP Tools Exposure to AgentSession and Model Context", async () => {
		const tools = await client.listTools();
		const echoDef = tools.find((t) => t.name === "echo_message")!;
		const agentTool = bridgeMcpTool(client, echoDef);

		const { session } = await createAgentSession({
			sessionManager: SessionManager.inMemory(),
			customTools: [agentTool],
			tools: [agentTool.name],
		});

		const activeTools = session.getActiveToolNames();
		expect(activeTools).toContain("mcp__test-fixture__echo_message");

		const modelTools = session.agent.state.tools;
		const exposed = modelTools.find((t) => t.name === "mcp__test-fixture__echo_message");
		expect(exposed).toBeDefined();
		expect(exposed?.description).toBe(echoDef.description);
	});
});
