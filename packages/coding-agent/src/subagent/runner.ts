/**
 * Subagent Session Runner for DUM-E
 * Executes isolated child AgentSessions with separated conversation context and resource boundaries.
 */

import type { AgentMessage } from "@dum-e/agent-core";
import { ModelRuntime } from "../core/model-runtime.ts";
import { createAgentSession } from "../core/sdk.ts";
import { SessionManager } from "../core/session-manager.ts";
import type { SubagentLaunchConfig } from "./types.ts";

export interface SubagentExecutionResult {
	resultText?: string;
	errorText?: string;
	cancelled?: boolean;
}

export async function executeChildSession(
	config: SubagentLaunchConfig,
	signal?: AbortSignal,
): Promise<SubagentExecutionResult> {
	if (signal?.aborted) {
		return { cancelled: true, errorText: "Execution aborted before starting." };
	}

	try {
		// Isolated in-memory session: strictly separates child from parent conversation context
		const sessionManager = SessionManager.inMemory();

		const promptText = config.agentDefinition
			? `[AGENT ROLE & INSTRUCTIONS]\n${config.agentDefinition}\n\n[TASK]\n${config.task}`
			: config.task;

		let modelRuntime = config.modelRuntime;
		if (!modelRuntime && config.authStorage) {
			modelRuntime = await ModelRuntime.create({
				credentials: config.authStorage,
				modelsPath: null,
				allowModelNetwork: false,
			});
		}

		const { session } = await createAgentSession({
			sessionManager,
			modelRuntime,
			model: config.model,
			tools: config.allowedTools,
			cwd: config.cwd,
		});

		// Listen for external abort signal and race with prompt execution
		let abortHandler: (() => void) | undefined;
		const abortPromise = new Promise<never>((_, reject) => {
			if (signal?.aborted) {
				reject(new Error("Subagent execution was cancelled."));
				return;
			}
			abortHandler = () => {
				session.abort().catch(() => {});
				reject(new Error("Subagent execution was cancelled."));
			};
			signal?.addEventListener("abort", abortHandler, { once: true });
		});

		try {
			await Promise.race([session.prompt(promptText), abortPromise]);
		} catch (err: any) {
			if (signal?.aborted) {
				return { cancelled: true, errorText: "Subagent execution was cancelled." };
			}
			throw err;
		} finally {
			if (signal && abortHandler) {
				signal.removeEventListener("abort", abortHandler);
			}
		}

		if (signal?.aborted) {
			return { cancelled: true, errorText: "Subagent execution was cancelled." };
		}

		// Collect text output from assistant messages in the child session
		const messages: AgentMessage[] = session.agent.state.messages;
		const assistantTexts: string[] = [];

		for (const msg of messages) {
			if (msg.role === "assistant" && Array.isArray(msg.content)) {
				for (const block of msg.content) {
					if (block.type === "text" && typeof block.text === "string") {
						assistantTexts.push(block.text);
					}
				}
			}
		}

		const resultText = assistantTexts.join("\n\n").trim();
		return {
			resultText: resultText.length > 0 ? resultText : "(Subagent completed with no text output)",
		};
	} catch (err: any) {
		if (signal?.aborted) {
			return { cancelled: true, errorText: "Subagent execution was cancelled." };
		}
		return {
			errorText: `Subagent session error: ${err.message || String(err)}`,
		};
	}
}
