import { fauxAssistantMessage, registerFauxProvider } from "@dum-e/ai/compat";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.ts";
import { SubagentManager } from "../src/subagent/manager.ts";
import { createSubagentTool } from "../src/subagent/tool.ts";
import { createInMemoryModelRegistry, getModelRuntime } from "./model-runtime-test-utils.ts";

describe("DUM-E Subagent System (HARNESS-DESIGN.md §3, §6 & DUM-E-IMPLEMENTATION.md §11)", () => {
	let manager: SubagentManager;
	let faux: ReturnType<typeof registerFauxProvider>;

	beforeEach(async () => {
		faux = registerFauxProvider({
			models: [{ id: "faux-subagent-1", reasoning: false }],
		});

		const model = faux.getModel();
		const authStorage = AuthStorage.inMemory();
		await authStorage.modify(model.provider, async () => ({
			type: "api_key",
			key: "faux-test-key",
		}));

		const modelRegistry = await createInMemoryModelRegistry(authStorage);
		modelRegistry.registerProvider(model.provider, {
			baseUrl: model.baseUrl,
			apiKey: "faux-test-key",
			api: faux.api,
			models: faux.models.map((m) => ({
				id: m.id,
				name: m.name,
				api: m.api,
				reasoning: m.reasoning,
				input: m.input,
				cost: m.cost,
				contextWindow: m.contextWindow,
				maxTokens: m.maxTokens,
				baseUrl: m.baseUrl,
			})),
		});

		const modelRuntime = getModelRuntime(modelRegistry);
		manager = new SubagentManager(1, { modelRuntime, defaultModel: model });
	});

	afterEach(() => {
		manager.close();
	});

	test("Subagent Lifecycle: Start, Await and Result Retrieval", async () => {
		faux.setResponses([fauxAssistantMessage("Subagent successfully built authentication module.")]);
		const model = faux.getModel();

		const record = await manager.start({
			task: "Build authentication module",
			agentDefinition: "You are a backend security engineer.",
			model,
		});

		expect(record.id).toMatch(/^subagent_/);
		expect(record.status).toBe("running");

		const awaited = await manager.awaitSubagents([record.id], 5000);
		if (awaited[0].errorText) {
			console.error("Subagent errorText:", awaited[0].errorText);
		}
		expect(awaited.length).toBe(1);
		expect(awaited[0].status).toBe("completed");
		expect(awaited[0].resultText).toContain("Subagent successfully built authentication module.");
		expect(awaited[0].durationMs).toBeGreaterThanOrEqual(0);
	});

	test("Subagent Cancellation via AbortController", async () => {
		// Mock slow response
		faux.setResponses([
			async () => {
				await new Promise((resolve) => setTimeout(resolve, 2000));
				return fauxAssistantMessage("Delayed response");
			},
		]);

		const record = await manager.start({
			task: "Long running calculation",
			model: faux.getModel(),
		});

		expect(record.status).toBe("running");

		const cancelled = manager.cancel(record.id);
		expect(cancelled).toBe(true);

		const inspected = manager.inspect(record.id);
		expect(inspected?.status).toBe("cancelled");
		expect(inspected?.errorText).toContain("cancelled");
	});

	test("Subagent Timeout Handling", async () => {
		faux.setResponses([
			async () => {
				await new Promise((resolve) => setTimeout(resolve, 3000));
				return fauxAssistantMessage("Late response");
			},
		]);

		const record = await manager.start({
			task: "Hanging operation",
			timeoutMs: 50,
			model: faux.getModel(),
		});

		const awaited = await manager.awaitSubagents([record.id], 2000);
		expect(awaited[0].status).toBe("cancelled");
	});

	test("Epoch Fencing: Outdated Subagent Attempt Cannot Supersede Active Epoch", async () => {
		faux.setResponses([
			async () => {
				await new Promise((resolve) => setTimeout(resolve, 100));
				return fauxAssistantMessage("Late epoch 1 completion");
			},
		]);

		const record = await manager.start({
			task: "Epoch 1 task",
			epoch: 1,
			model: faux.getModel(),
		});

		// Coordinator advances epoch to 2 while subagent is running
		manager.setEpoch(2);

		const awaited = await manager.awaitSubagents([record.id], 2000);
		expect(awaited[0].status).toBe("cancelled");
		expect(awaited[0].errorText).toContain("Epoch fencing");
	});

	test("Subagent Tool Interface (start, list, inspect, await, cancel)", async () => {
		faux.setResponses([fauxAssistantMessage("Database migration completed.")]);
		const tool = createSubagentTool(manager);

		// 1. Action: start
		const startRes = await tool.execute("call-start", {
			action: "start",
			task: "Run DB migrations",
			agent: "DBA Specialist",
		});
		expect((startRes.details as any)?.success).toBe(true);
		const subagentId = (startRes.details as any)?.subagent?.id;
		expect(subagentId).toBeDefined();

		// 2. Action: list
		const listRes = await tool.execute("call-list", { action: "list" });
		expect((listRes.details as any)?.success).toBe(true);
		expect((listRes.content[0] as any).text).toContain(subagentId);

		// 3. Action: inspect
		const inspectRes = await tool.execute("call-inspect", {
			action: "inspect",
			id: subagentId,
		});
		expect((inspectRes.details as any)?.success).toBe(true);
		expect((inspectRes.details as any)?.subagent?.id).toBe(subagentId);

		// 4. Action: await
		const awaitRes = await tool.execute("call-await", {
			action: "await",
			ids: [subagentId],
			timeout_ms: 5000,
		});
		expect((awaitRes.details as any)?.success).toBe(true);
		expect((awaitRes.content[0] as any).text).toContain("completed");

		// 5. Action: cancel
		const cancelTaskRes = await tool.execute("call-start-cancel", {
			action: "start",
			task: "Task to cancel",
		});
		const cancelId = (cancelTaskRes.details as any)?.subagent?.id;
		const cancelRes = await tool.execute("call-cancel", {
			action: "cancel",
			id: cancelId,
		});
		expect((cancelRes.details as any)?.success).toBe(true);
	});
});
