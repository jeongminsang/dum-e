/**
 * Subagent Tool for DUM-E
 * Exposes subagent control (start, list, inspect, await, cancel) to the agent tool registry.
 */

import type { AgentTool, AgentToolResult } from "@dum-e/agent-core";
import { Type } from "typebox";
import type { SubagentManager } from "./manager.ts";
import type { SubagentToolParams } from "./types.ts";

export function createSubagentTool(manager: SubagentManager): AgentTool {
	return {
		name: "subagent",
		label: "Subagents",
		description:
			"Control LLM subagents: start isolated tasks, list running agents, inspect status/logs, await completion, or cancel execution.",
		parameters: Type.Object({
			action: Type.Union(
				[
					Type.Literal("start"),
					Type.Literal("list"),
					Type.Literal("inspect"),
					Type.Literal("await"),
					Type.Literal("cancel"),
				],
				{ description: "Subagent action to perform" },
			),
			id: Type.Optional(Type.String({ description: "Target subagent ID for inspect or cancel" })),
			ids: Type.Optional(Type.Array(Type.String(), { description: "Subagent IDs to await" })),
			task: Type.Optional(Type.String({ description: "Task prompt/instructions (required for start)" })),
			agent: Type.Optional(Type.String({ description: "Role definition/system prompt for subagent" })),
			timeout_ms: Type.Optional(Type.Number({ description: "Timeout in milliseconds when awaiting" })),
		}),
		execute: async (_callId: string, rawParams: any, _signal?: AbortSignal): Promise<AgentToolResult<unknown>> => {
			const params = rawParams as SubagentToolParams;
			switch (params.action) {
				case "start": {
					if (!params.task) {
						return {
							content: [{ type: "text", text: "Error: 'task' parameter is required for start action." }],
							details: { success: false, error: "Missing required 'task' parameter" },
						};
					}

					const record = await manager.start({
						task: params.task,
						agentDefinition: params.agent,
						timeoutMs: params.timeout_ms,
					});

					return {
						content: [
							{
								type: "text",
								text: `Subagent started successfully:\nID: ${record.id}\nStatus: ${record.status}\nEpoch: ${record.epoch}\nTask: ${record.task}`,
							},
						],
						details: { success: true, subagent: record },
					};
				}

				case "list": {
					const list = manager.list();
					if (list.length === 0) {
						return {
							content: [{ type: "text", text: "No subagents currently registered." }],
							details: { success: true, subagents: [] },
						};
					}

					const text = list
						.map(
							(s) =>
								`• [${s.id}] Status: ${s.status} (Duration: ${s.durationMs}ms) | Task: ${s.task.slice(0, 60)}`,
						)
						.join("\n");

					return {
						content: [{ type: "text", text: `Active and historical subagents (${list.length}):\n${text}` }],
						details: { success: true, subagents: list },
					};
				}

				case "inspect": {
					if (!params.id) {
						return {
							content: [{ type: "text", text: "Error: 'id' parameter is required for inspect action." }],
							details: { success: false, error: "Missing required 'id' parameter" },
						};
					}

					const record = manager.inspect(params.id);
					if (!record) {
						return {
							content: [{ type: "text", text: `Subagent "${params.id}" not found.` }],
							details: { success: false, error: "Not found" },
						};
					}

					let detailText = `Subagent Details for ${record.id}:\nStatus: ${record.status}\nEpoch: ${record.epoch}\nDuration: ${record.durationMs}ms\nTask: ${record.task}`;
					if (record.resultText) {
						detailText += `\n\n--- Result ---\n${record.resultText}`;
					}
					if (record.errorText) {
						detailText += `\n\n--- Error ---\n${record.errorText}`;
					}

					return {
						content: [{ type: "text", text: detailText }],
						details: { success: true, subagent: record },
					};
				}

				case "await": {
					const records = await manager.awaitSubagents(params.ids, params.timeout_ms ?? 30000);
					const summary = records
						.map(
							(r) =>
								`• [${r.id}] Status: ${r.status}${r.resultText ? ` -> ${r.resultText.slice(0, 80)}...` : ""}`,
						)
						.join("\n");

					return {
						content: [{ type: "text", text: `Subagent await completed:\n${summary}` }],
						details: { success: true, subagents: records },
					};
				}

				case "cancel": {
					if (!params.id) {
						return {
							content: [{ type: "text", text: "Error: 'id' parameter is required for cancel action." }],
							details: { success: false, error: "Missing required 'id' parameter" },
						};
					}

					const ok = manager.cancel(params.id);
					return {
						content: [
							{
								type: "text",
								text: ok
									? `Subagent "${params.id}" successfully cancelled.`
									: `Subagent "${params.id}" not found or already terminal.`,
							},
						],
						details: { success: ok },
					};
				}

				default:
					return {
						content: [{ type: "text", text: `Unknown subagent action: ${(params as any).action}` }],
						details: { success: false, error: "Unknown action" },
					};
			}
		},
	};
}
