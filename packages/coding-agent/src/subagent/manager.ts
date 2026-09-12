/**
 * Subagent Manager for DUM-E
 * Coordinates subagent lifecycle, async tracking, await/timeout, cancellation, and epoch fencing.
 */

import * as crypto from "node:crypto";
import type { Model } from "@dum-e/ai";
import type { AuthStorage } from "../core/auth-storage.ts";
import type { ModelRuntime } from "../core/model-runtime.ts";
import { executeChildSession, type SubagentExecutionResult } from "./runner.ts";
import type { SubagentLaunchConfig, SubagentManagerOptions, SubagentRecord, SubagentStatus } from "./types.ts";

interface ActiveSubagentJob {
	abortController: AbortController;
	promise: Promise<void>;
	timeoutTimer?: NodeJS.Timeout;
}

export class SubagentManager {
	public currentEpoch: number = 1;
	private records = new Map<string, SubagentRecord>();
	private activeJobs = new Map<string, ActiveSubagentJob>();
	private modelRuntime?: ModelRuntime;
	private authStorage?: AuthStorage;
	private defaultModel?: Model<any>;

	constructor(initialEpoch: number = 1, options?: SubagentManagerOptions) {
		this.currentEpoch = initialEpoch;
		this.modelRuntime = options?.modelRuntime;
		this.authStorage = options?.authStorage;
		this.defaultModel = options?.defaultModel;
	}

	setEpoch(newEpoch: number): void {
		this.currentEpoch = newEpoch;
	}

	async start(config: SubagentLaunchConfig): Promise<SubagentRecord> {
		const id = `subagent_${crypto.randomUUID().slice(0, 8)}`;
		const epoch = config.epoch ?? this.currentEpoch;
		const now = Date.now();

		const effectiveConfig: SubagentLaunchConfig = {
			...config,
			model: config.model ?? this.defaultModel,
			modelRuntime: config.modelRuntime ?? this.modelRuntime,
			authStorage: config.authStorage ?? this.authStorage,
		};

		const record: SubagentRecord = {
			id,
			task: effectiveConfig.task,
			agentDefinition: effectiveConfig.agentDefinition,
			status: "running",
			epoch,
			createdAt: now,
			startedAt: now,
			durationMs: 0,
		};

		this.records.set(id, record);

		const abortController = new AbortController();
		let timeoutTimer: NodeJS.Timeout | undefined;

		if (effectiveConfig.timeoutMs && effectiveConfig.timeoutMs > 0) {
			timeoutTimer = setTimeout(() => {
				abortController.abort();
				if (record.status === "running") {
					record.status = "cancelled";
					record.completedAt = Date.now();
					record.durationMs = record.completedAt - record.createdAt;
					record.errorText = `Subagent timed out after ${effectiveConfig.timeoutMs}ms`;
				}
			}, effectiveConfig.timeoutMs);
		}

		const promise = (async () => {
			try {
				const execResult: SubagentExecutionResult = await executeChildSession(
					effectiveConfig,
					abortController.signal,
				);

				const finishTime = Date.now();
				record.completedAt = finishTime;
				record.durationMs = finishTime - record.createdAt;

				// Epoch fencing: verify that epoch is still valid
				if (record.epoch < this.currentEpoch) {
					record.status = "cancelled";
					record.errorText = `Epoch fencing: attempt epoch ${record.epoch} superseded by active epoch ${this.currentEpoch}`;
					return;
				}

				if (execResult.cancelled) {
					record.status = "cancelled";
					record.errorText = execResult.errorText ?? "Execution cancelled";
				} else if (execResult.errorText) {
					record.status = "failed";
					record.errorText = execResult.errorText;
				} else {
					record.status = "completed";
					record.resultText = execResult.resultText;
				}
			} catch (err: any) {
				record.completedAt = Date.now();
				record.durationMs = record.completedAt - record.createdAt;
				record.status = "failed";
				record.errorText = err.message || String(err);
			} finally {
				if (timeoutTimer) clearTimeout(timeoutTimer);
				this.activeJobs.delete(id);
			}
		})();

		this.activeJobs.set(id, {
			abortController,
			promise,
			timeoutTimer,
		});

		return { ...record };
	}

	list(filterStatus?: SubagentStatus): SubagentRecord[] {
		const all = Array.from(this.records.values());
		if (!filterStatus) return all;
		return all.filter((r) => r.status === filterStatus);
	}

	inspect(id: string): SubagentRecord | undefined {
		const rec = this.records.get(id);
		return rec ? { ...rec } : undefined;
	}

	async awaitSubagents(ids?: string[], timeoutMs: number = 30000): Promise<SubagentRecord[]> {
		const targetIds = ids && ids.length > 0 ? ids : Array.from(this.activeJobs.keys());
		const promisesToWait = targetIds
			.map((id) => this.activeJobs.get(id)?.promise)
			.filter((p): p is Promise<void> => p !== undefined);

		if (promisesToWait.length === 0) {
			return targetIds.map((id) => this.inspect(id)!).filter(Boolean);
		}

		const waitAll = Promise.allSettled(promisesToWait);

		if (timeoutMs > 0) {
			let timer: NodeJS.Timeout;
			const timeoutPromise = new Promise<void>((_, reject) => {
				timer = setTimeout(() => {
					reject(new Error(`Timed out waiting for subagents after ${timeoutMs}ms`));
				}, timeoutMs);
			});

			try {
				await Promise.race([waitAll, timeoutPromise]);
			} catch {
				// Timed out: return current snapshot without throwing out the whole list
			} finally {
				clearTimeout(timer!);
			}
		} else {
			await waitAll;
		}

		return targetIds.map((id) => this.inspect(id)!).filter(Boolean);
	}

	cancel(id: string): boolean {
		const job = this.activeJobs.get(id);
		const record = this.records.get(id);

		if (job) {
			job.abortController.abort();
			if (job.timeoutTimer) clearTimeout(job.timeoutTimer);
			this.activeJobs.delete(id);
		}

		if (record && record.status === "running") {
			record.status = "cancelled";
			record.completedAt = Date.now();
			record.durationMs = record.completedAt - record.createdAt;
			record.errorText = "Subagent was cancelled by user or coordinator.";
			return true;
		}

		return Boolean(job);
	}

	close(): void {
		for (const [id, job] of this.activeJobs) {
			job.abortController.abort();
			if (job.timeoutTimer) clearTimeout(job.timeoutTimer);
			const rec = this.records.get(id);
			if (rec && rec.status === "running") {
				rec.status = "cancelled";
			}
		}
		this.activeJobs.clear();
	}
}
