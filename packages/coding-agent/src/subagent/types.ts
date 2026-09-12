/**
 * Subagent System Type Definitions for DUM-E
 * Conforms to HARNESS-DESIGN.md & DUM-E-IMPLEMENTATION.md §11
 */

import type { Model } from "@dum-e/ai";
import type { AuthStorage } from "../core/auth-storage.ts";
import type { ModelRuntime } from "../core/model-runtime.ts";

export type SubagentStatus = "queued" | "running" | "completed" | "failed" | "cancelled";

export interface SubagentLaunchConfig {
	task: string;
	agentDefinition?: string;
	model?: Model<any>;
	allowedTools?: string[];
	cwd?: string;
	timeoutMs?: number;
	epoch?: number;
	modelRuntime?: ModelRuntime;
	authStorage?: AuthStorage;
}

export interface SubagentManagerOptions {
	modelRuntime?: ModelRuntime;
	authStorage?: AuthStorage;
	defaultModel?: Model<any>;
}

export interface SubagentRecord {
	id: string;
	task: string;
	agentDefinition?: string;
	status: SubagentStatus;
	epoch: number;
	createdAt: number;
	startedAt?: number;
	completedAt?: number;
	durationMs: number;
	resultText?: string;
	errorText?: string;
}

export type SubagentAction = "start" | "list" | "inspect" | "await" | "cancel";

export interface SubagentToolParams {
	action: SubagentAction;
	id?: string;
	ids?: string[];
	task?: string;
	agent?: string;
	timeout_ms?: number;
}
