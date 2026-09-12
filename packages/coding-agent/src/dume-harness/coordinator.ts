/**
 * DUM-E Harness Coordinator
 * Single authority over task scheduling, worker lifecycle, epoch fencing,
 * restarts, and fault recovery.
 * Conforms to HARNESS-DESIGN.md §3, §6, §7 & DUM-E-IMPLEMENTATION.md §11
 */

import * as crypto from "node:crypto";
import { getRepoRoot, integrateCandidateCommit } from "./git.ts";
import type { HarnessStore } from "./store.ts";
import type { AttemptRecord, IntegrationRecord, ResultManifest, TaskRecord, VerificationRecord } from "./types.ts";

export interface WorkerRunner {
	id: string;
	runAttempt(attempt: AttemptRecord, task: TaskRecord): Promise<ResultManifest>;
	cancelAttempt(attemptId: string): Promise<void>;
}

export class DumeCoordinator {
	public store: HarnessStore;
	public readonly coordinatorId: string;
	public epoch: number = 1;
	public repoRoot?: string | null;
	private heartbeatTimer?: NodeJS.Timeout;
	private workers: Map<string, WorkerRunner> = new Map();
	private activeJobs: Map<string, Promise<void>> = new Map();

	constructor(store: HarnessStore, coordinatorId?: string, repoRoot?: string | null) {
		this.store = store;
		this.coordinatorId = coordinatorId ?? `coord_${crypto.randomUUID().slice(0, 8)}`;
		this.repoRoot = repoRoot;
	}

	async start(): Promise<void> {
		if (this.repoRoot === undefined) {
			try {
				this.repoRoot = (await getRepoRoot(process.cwd())) ?? undefined;
			} catch {
				// No git repo detected in cwd
			}
		}

		const lock = this.store.acquireCoordinatorLock(this.coordinatorId, 15000);
		if (!lock.acquired) {
			throw new Error(`Failed to acquire Coordinator primary lock. Active epoch is ${lock.epoch}`);
		}
		this.epoch = lock.epoch;

		this.heartbeatTimer = setInterval(() => {
			try {
				const ok = this.store.heartbeatCoordinatorLock(this.coordinatorId, this.epoch);
				if (!ok) {
					console.error("[DUM-E Coordinator] Lost primary lock lease!");
				}
			} catch {
				// Store might be closed or errored
				if (this.heartbeatTimer) clearInterval(this.heartbeatTimer);
			}
		}, 5000);

		await this.recoverState();
	}

	async stop(): Promise<void> {
		if (this.heartbeatTimer) {
			clearInterval(this.heartbeatTimer);
			this.heartbeatTimer = undefined;
		}

		// Await any pending active jobs
		await Promise.allSettled(Array.from(this.activeJobs.values()));
		this.activeJobs.clear();

		try {
			this.store.releaseCoordinatorLock(this.coordinatorId, this.epoch);
		} catch {
			// Ignore if store is already closed
		}
	}

	registerWorker(worker: WorkerRunner): void {
		this.workers.set(worker.id, worker);
	}

	private async recoverState(): Promise<void> {
		const unfinished = this.store.getUnfinishedAttempts();
		const now = Date.now();

		for (const attempt of unfinished) {
			if (attempt.status === "result_submitted") {
				// Result was already submitted before crash; perform verification and integration
				await this.verifyAndIntegrate(attempt.id);
			} else if (now > attempt.leaseExpiresAt || attempt.coordinatorEpoch < this.epoch) {
				// Worker crashed or epoch was superseded by new coordinator
				console.warn(
					`[DUM-E Coordinator] Recovering stale attempt ${attempt.id} for task ${attempt.taskId}. Worker lease expired or epoch superseded.`,
				);
				this.store.updateTaskStatus(attempt.taskId, "needs_attention");
			}
		}
	}

	async dispatchTask(
		taskId: string,
		workerId: string,
		baseCommit: string,
		worktreePath: string,
	): Promise<AttemptRecord> {
		const task = this.store.getTask(taskId);
		if (!task) throw new Error(`Task ${taskId} not found`);

		for (const depId of task.dependencies) {
			const dep = this.store.getTask(depId);
			if (!dep || dep.status !== "completed") {
				throw new Error(`Dependency ${depId} is not completed (current: ${dep?.status})`);
			}
		}

		return this.store.createAttempt(taskId, this.epoch, workerId, baseCommit, worktreePath);
	}

	async executeAttempt(attemptId: string): Promise<ResultManifest | null> {
		const attempt = this.store.getAttempt(attemptId);
		if (!attempt) throw new Error(`Attempt ${attemptId} not found`);

		const task = this.store.getTask(attempt.taskId);
		if (!task) throw new Error(`Task ${attempt.taskId} not found`);

		const worker = this.workers.get(attempt.workerId);
		if (!worker) throw new Error(`Worker ${attempt.workerId} not registered`);

		this.store.heartbeatAttempt(attempt.id, attempt.epoch);
		const manifest = await worker.runAttempt(attempt, task);

		const submitRes = this.store.submitResultManifest(manifest, attempt.epoch);
		if (!submitRes.accepted) {
			console.error(`[DUM-E Coordinator] Result rejected: ${submitRes.reason}`);
			return null;
		}

		await this.verifyAndIntegrate(attempt.id);
		return manifest;
	}

	async verifyAndIntegrate(attemptId: string): Promise<boolean> {
		const attempt = this.store.getAttempt(attemptId);
		if (!attempt) return false;

		const task = this.store.getTask(attempt.taskId);
		if (!task) return false;

		const manifest = this.store.getResultManifest(attemptId);
		if (!manifest) return false;

		const goal = this.store.getGoal(task.goalId);
		if (!goal) return false;

		// 1. Allowed paths whitelist check
		for (const modFile of manifest.modifiedFiles) {
			const isAllowed = task.allowedPaths.some((prefix) => modFile.startsWith(prefix));
			if (!isAllowed) {
				console.error(
					`[DUM-E Verification] File ${modFile} violates allowedPaths whitelist [${task.allowedPaths.join(", ")}]!`,
				);
				this.store.updateTaskStatus(task.id, "failed");
				return false;
			}
		}

		// 2. Acceptance Verification execution
		// STRICT: Never default missing tests to true!
		const testPassed = Boolean(manifest.testResults && manifest.testResults.passed === true);
		const verificationId = `ver_${crypto.randomUUID().slice(0, 10)}`;
		const logHash = manifest.executionLogsHash ?? "hash_placeholder";

		const verRecord: VerificationRecord = {
			id: verificationId,
			taskId: task.id,
			attemptId,
			manifestHash: crypto.createHash("sha256").update(JSON.stringify(manifest)).digest("hex"),
			requirementsRevision: goal.requirementsRevision,
			command: manifest.testResults?.command ?? "none",
			passed: testPassed,
			logsHash: logHash,
			verifiedAt: Date.now(),
		};
		this.store.recordVerification(verRecord);

		if (!testPassed) {
			console.warn(
				`[DUM-E Verification] Verification failed for task ${task.id}: tests did not pass or were omitted.`,
			);
			this.store.updateTaskStatus(task.id, "failed");
			return false;
		}

		// 3. Serialized Integration
		const intId = `int_${crypto.randomUUID().slice(0, 10)}`;
		const candidateCommit = manifest.candidateCommit;

		// If candidate commit exists and we have repoRoot, perform real git integration
		let integrationStatus: IntegrationRecord["status"] = "failed";
		let conflictDetails: string | undefined;

		if (candidateCommit && typeof this.repoRoot === "string") {
			const intResult = await integrateCandidateCommit(this.repoRoot, "main", candidateCommit);
			if (intResult.success) {
				integrationStatus = "applied";
			} else {
				integrationStatus = intResult.conflict ? "conflict" : "failed";
				conflictDetails = intResult.error;
			}
		} else if (candidateCommit && this.repoRoot === null) {
			// Candidate commit is present and repoRoot is explicitly null (mock test environment)
			integrationStatus = "applied";
		} else {
			// No candidate commit was created or git repo not configured
			integrationStatus = "failed";
			conflictDetails = "No candidate commit provided for integration or repository root not configured.";
		}

		const integration: IntegrationRecord = {
			id: intId,
			goalId: goal.id,
			taskId: task.id,
			targetBranch: "main",
			baseCommit: manifest.baseCommit,
			candidateCommit: candidateCommit ?? "none",
			status: integrationStatus,
			conflictDetails,
			integratedAt: integrationStatus === "applied" ? Date.now() : undefined,
		};
		this.store.recordIntegration(integration);

		if (integrationStatus !== "applied") {
			console.warn(`[DUM-E Integration] Integration failed for task ${task.id}: ${conflictDetails}`);
			this.store.updateTaskStatus(task.id, "failed");
			return false;
		}

		this.store.updateTaskStatus(task.id, "completed");

		// Check if goal fully completed
		const allTasks = this.store.getTasksByGoal(goal.id);
		const allCompleted = allTasks.every((t) => t.status === "completed");
		if (allCompleted) {
			this.store.updateGoalStatus(goal.id, "completed", Date.now());
			console.log(`[DUM-E Goal Complete] Goal ${goal.id} successfully finished!`);
		}

		return true;
	}

	async cancelTask(taskId: string): Promise<void> {
		const task = this.store.getTask(taskId);
		if (!task) return;

		this.store.updateTaskStatus(taskId, "cancelled");
		if (task.activeAttemptId) {
			const attempt = this.store.getAttempt(task.activeAttemptId);
			if (attempt) {
				const worker = this.workers.get(attempt.workerId);
				if (worker) {
					await worker.cancelAttempt(attempt.id);
				}
			}
		}
	}
}
