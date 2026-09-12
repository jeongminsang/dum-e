/**
 * DUM-E Harness Worker Host & Worktree Sandbox Management
 * Provides isolated Git worktree environments, artifact creation, and execution boundaries.
 * Conforms to HARNESS-DESIGN.md §3, §6 & DUM-E-IMPLEMENTATION.md §11
 */

import { exec } from "node:child_process";
import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import { promisify } from "node:util";
import type { WorkerRunner } from "./coordinator.ts";
import {
	commitWorktreeChanges,
	createGitWorktree,
	getRepoRoot,
	getWorktreeModifiedFiles,
	removeGitWorktree,
} from "./git.ts";
import type { HarnessStore } from "./store.ts";
import type { AttemptRecord, ResultManifest, TaskRecord } from "./types.ts";

const execAsync = promisify(exec);

export type AgentTaskRunner = (
	worktreePath: string,
	task: TaskRecord,
	attempt: AttemptRecord,
	signal: AbortSignal,
) => Promise<void>;

export interface DumeWorkerHostOptions {
	repoRoot?: string;
	worktreeBaseDir?: string;
	agentRunner?: AgentTaskRunner;
	testCommand?: string;
}

interface ActiveAttemptState {
	abortController: AbortController;
	worktreePath: string;
}

export class DumeWorkerHost implements WorkerRunner {
	public readonly id: string;
	private store: HarnessStore;
	private repoRoot?: string;
	private worktreeBaseDir: string;
	private agentRunner?: AgentTaskRunner;
	private testCommand?: string;
	private activeAttempts = new Map<string, ActiveAttemptState>();

	constructor(id: string, store: HarnessStore, options?: string | DumeWorkerHostOptions) {
		this.id = id;
		this.store = store;

		if (typeof options === "string") {
			this.worktreeBaseDir = options;
		} else {
			this.repoRoot = options?.repoRoot;
			this.worktreeBaseDir = options?.worktreeBaseDir ?? path.join(process.cwd(), ".dume", "worktrees");
			this.agentRunner = options?.agentRunner;
			this.testCommand = options?.testCommand;
		}

		if (!fs.existsSync(this.worktreeBaseDir)) {
			fs.mkdirSync(this.worktreeBaseDir, { recursive: true });
		}
	}

	async prepareWorktree(attemptId: string, baseCommit: string): Promise<string> {
		const wtPath = path.join(this.worktreeBaseDir, attemptId);

		const resolvedRepoRoot = this.repoRoot ?? (await getRepoRoot(process.cwd()));
		if (!resolvedRepoRoot) {
			throw new Error(`Cannot prepare Git worktree: repository root not found for attempt ${attemptId}.`);
		}
		this.repoRoot = resolvedRepoRoot;

		await createGitWorktree(this.repoRoot, wtPath, baseCommit);
		return wtPath;
	}

	async runAttempt(attempt: AttemptRecord, task: TaskRecord): Promise<ResultManifest> {
		const abortController = new AbortController();

		let wtPath = attempt.worktreePath;
		if (!wtPath || !fs.existsSync(wtPath)) {
			wtPath = await this.prepareWorktree(attempt.id, attempt.baseCommit);
		}

		this.activeAttempts.set(attempt.id, {
			abortController,
			worktreePath: wtPath,
		});

		try {
			// 1. Run the real agent runner if configured
			if (this.agentRunner) {
				await this.agentRunner(wtPath, task, attempt, abortController.signal);
			}

			if (abortController.signal.aborted) {
				throw new Error(`Attempt ${attempt.id} was aborted during execution`);
			}

			// 2. Discover modified files
			let modifiedFiles: string[] = [];
			try {
				modifiedFiles = await getWorktreeModifiedFiles(wtPath);
			} catch {
				// Fallback to checking allowed paths on disk if not a git worktree
				for (const allowed of task.allowedPaths) {
					const target = path.join(wtPath, allowed);
					if (fs.existsSync(target)) {
						modifiedFiles.push(allowed);
					}
				}
			}

			// 3. Save artifact hashes for modified files
			const changedArtifacts: Record<string, string> = {};
			for (const modFile of modifiedFiles) {
				const fullPath = path.join(wtPath, modFile);
				if (fs.existsSync(fullPath) && fs.statSync(fullPath).isFile()) {
					const content = fs.readFileSync(fullPath, "utf-8");
					const saved = this.store.saveArtifact(content);
					changedArtifacts[modFile] = saved.hash;
				}
			}

			// 4. Commit changes in worktree to get candidate commit
			let candidateCommit: string | undefined;
			if (modifiedFiles.length > 0 && this.repoRoot) {
				try {
					candidateCommit = await commitWorktreeChanges(
						wtPath,
						`feat(dume): task ${task.id} attempt ${attempt.id} (epoch ${attempt.epoch})`,
					);
				} catch (err) {
					console.warn(`[DUM-E Worker] Worktree commit warning:`, err);
				}
			}

			// 5. Execute real test command if configured
			const effectiveTestCommand = task.inputManifest.testCommand ?? this.testCommand;

			let testResults: ResultManifest["testResults"] | undefined;
			let logContent = `Attempt ${attempt.id} executed for task ${task.id}.\n`;

			if (effectiveTestCommand) {
				try {
					const { stdout, stderr } = await execAsync(effectiveTestCommand, {
						cwd: wtPath,
						signal: abortController.signal,
						timeout: 120_000,
					});
					logContent += `Test Command: ${effectiveTestCommand}\nSTDOUT:\n${stdout}\nSTDERR:\n${stderr}\n`;
					const outputHash = crypto
						.createHash("sha256")
						.update(stdout + stderr)
						.digest("hex");
					testResults = {
						passed: true,
						command: effectiveTestCommand,
						outputHash,
					};
				} catch (testErr: any) {
					const stdout = testErr.stdout ? String(testErr.stdout) : "";
					const stderr = testErr.stderr ? String(testErr.stderr) : testErr.message;
					logContent += `Test FAILED: ${effectiveTestCommand}\nSTDOUT:\n${stdout}\nSTDERR:\n${stderr}\n`;
					const outputHash = crypto
						.createHash("sha256")
						.update(stdout + stderr)
						.digest("hex");
					testResults = {
						passed: false,
						command: effectiveTestCommand,
						outputHash,
					};
				}
			}

			const logSaved = this.store.saveArtifact(logContent);

			const manifest: ResultManifest = {
				attemptId: attempt.id,
				taskId: task.id,
				baseCommit: attempt.baseCommit,
				candidateCommit,
				changedArtifacts,
				modifiedFiles,
				executionLogsHash: logSaved.hash,
				testResults,
			};

			return manifest;
		} finally {
			this.activeAttempts.delete(attempt.id);
		}
	}

	async cancelAttempt(attemptId: string): Promise<void> {
		const active = this.activeAttempts.get(attemptId);
		if (active) {
			active.abortController.abort();
			if (this.repoRoot && active.worktreePath) {
				try {
					await removeGitWorktree(this.repoRoot, active.worktreePath);
				} catch (err) {
					console.warn(`[DUM-E Worker] Error cleaning up worktree on cancel:`, err);
				}
			}
			this.activeAttempts.delete(attemptId);
		}
	}
}
