import { execSync } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { DumeCoordinator, DumeWorkerHost, HarnessStore } from "../src/dume-harness/index.ts";

describe("DUM-E Harness on Clean Pi Base (HARNESS-DESIGN.md & DUM-E-IMPLEMENTATION.md)", () => {
	let tempDir: string;
	let dbPath: string;
	let store: HarnessStore;
	let coordinator: DumeCoordinator;

	beforeEach(async () => {
		tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "dume-clean-"));
		dbPath = path.join(tempDir, "dume.db");
		store = new HarnessStore(dbPath, path.join(tempDir, "artifacts"));
		coordinator = new DumeCoordinator(store, "coord-pi-1", null);
		await coordinator.start();
	});

	afterEach(async () => {
		await coordinator.stop();
		store.close();
		fs.rmSync(tempDir, { recursive: true, force: true });
	});

	test("Stage 1 & 2: Goal, DAG Tasks, Epoch Fencing and Result Submission", async () => {
		const goal = store.insertGoal({
			id: "goal-clean-1",
			title: "Build Distributed Service",
			requirements: "Implement fault tolerant microservice",
			requirementsRevision: 1,
			acceptanceCriteria: ["All tests pass", "Zero crash on restart"],
			status: "active",
			userInterrupted: false,
		});
		expect(goal.id).toBe("goal-clean-1");
		expect(goal.status).toBe("active");

		const _task1 = store.insertTask({
			id: "task-1",
			goalId: "goal-clean-1",
			title: "Setup schema",
			dependencies: [],
			inputManifest: {},
			allowedPaths: ["src/schema.ts"],
			retryLimit: 3,
			retryCount: 0,
			status: "ready",
		});

		const _task2 = store.insertTask({
			id: "task-2",
			goalId: "goal-clean-1",
			title: "Setup API handlers",
			dependencies: ["task-1"],
			inputManifest: {},
			allowedPaths: ["src/api.ts"],
			retryLimit: 3,
			retryCount: 0,
			status: "blocked",
		});

		const worker = new DumeWorkerHost("worker-clean-1", store, path.join(tempDir, "worktrees"));
		coordinator.registerWorker(worker);

		// DAG dependency violation check
		await expect(coordinator.dispatchTask("task-2", "worker-clean-1", "commit_0", "")).rejects.toThrow(
			"Dependency task-1 is not completed",
		);

		// Dispatch Task 1
		const attempt1 = await coordinator.dispatchTask("task-1", "worker-clean-1", "commit_0", "");
		expect(attempt1.taskId).toBe("task-1");
		expect(attempt1.epoch).toBe(1);

		// Epoch Fencing check
		const staleSubmit = store.submitResultManifest(
			{
				attemptId: attempt1.id,
				taskId: "task-1",
				baseCommit: "commit_0",
				changedArtifacts: {},
				modifiedFiles: ["src/schema.ts"],
			},
			999,
		);
		expect(staleSubmit.accepted).toBe(false);
		expect(staleSubmit.reason).toContain("Epoch mismatch");

		// Valid submit with real test results and candidate commit
		const validSubmit = store.submitResultManifest(
			{
				attemptId: attempt1.id,
				taskId: "task-1",
				baseCommit: "commit_0",
				candidateCommit: "candidate_commit_1",
				changedArtifacts: { "src/schema.ts": "hash_123" },
				modifiedFiles: ["src/schema.ts"],
				testResults: { passed: true, command: "bun test", outputHash: "hash_test" },
			},
			attempt1.epoch,
		);
		expect(validSubmit.accepted).toBe(true);

		const integrated1 = await coordinator.verifyAndIntegrate(attempt1.id);
		expect(integrated1).toBe(true);
		expect(store.getTask("task-1")?.status).toBe("completed");

		// Now Task 2 can run
		const attempt2 = await coordinator.dispatchTask("task-2", "worker-clean-1", "commit_0", "");
		const submit2 = store.submitResultManifest(
			{
				attemptId: attempt2.id,
				taskId: "task-2",
				baseCommit: "commit_0",
				candidateCommit: "candidate_commit_2",
				changedArtifacts: { "src/api.ts": "hash_456" },
				modifiedFiles: ["src/api.ts"],
				testResults: { passed: true, command: "bun test", outputHash: "hash_test_2" },
			},
			attempt2.epoch,
		);
		expect(submit2.accepted).toBe(true);

		await coordinator.verifyAndIntegrate(attempt2.id);
		expect(store.getTask("task-2")?.status).toBe("completed");
		expect(store.getGoal("goal-clean-1")?.status).toBe("completed");
	});

	test("Stage 4: AllowedPaths Whitelist Violation in Verification", async () => {
		store.insertGoal({
			id: "goal-clean-2",
			title: "Security sandbox test",
			requirements: "Safe operations",
			requirementsRevision: 1,
			acceptanceCriteria: [],
			status: "active",
			userInterrupted: false,
		});

		const _task = store.insertTask({
			id: "task-sec",
			goalId: "goal-clean-2",
			title: "Strict edit",
			dependencies: [],
			inputManifest: {},
			allowedPaths: ["src/safe/"],
			retryLimit: 1,
			retryCount: 0,
			status: "ready",
		});

		const worker = new DumeWorkerHost("worker-clean-2", store, path.join(tempDir, "worktrees"));
		coordinator.registerWorker(worker);

		const attempt = await coordinator.dispatchTask("task-sec", "worker-clean-2", "commit_0", "");

		store.submitResultManifest(
			{
				attemptId: attempt.id,
				taskId: "task-sec",
				baseCommit: "commit_0",
				candidateCommit: "candidate_sec",
				changedArtifacts: { "/etc/shadow": "bad_hash" },
				modifiedFiles: ["/etc/shadow"],
				testResults: { passed: true, command: "bun test", outputHash: "hash" },
			},
			attempt.epoch,
		);

		const pass = await coordinator.verifyAndIntegrate(attempt.id);
		expect(pass).toBe(false);
		expect(store.getTask("task-sec")?.status).toBe("failed");
	});

	test("Strict Verification: Missing tests or test failure MUST reject integration", async () => {
		store.insertGoal({
			id: "goal-clean-strict",
			title: "Strict test requirement",
			requirements: "Must run real tests",
			requirementsRevision: 1,
			acceptanceCriteria: ["Tests pass"],
			status: "active",
			userInterrupted: false,
		});

		store.insertTask({
			id: "task-no-test",
			goalId: "goal-clean-strict",
			title: "Task with missing test",
			dependencies: [],
			inputManifest: {},
			allowedPaths: ["src/code.ts"],
			retryLimit: 1,
			retryCount: 0,
			status: "ready",
		});

		const attemptNoTest = await coordinator.dispatchTask("task-no-test", "worker-1", "commit_0", "");

		// 1. Submit without testResults
		store.submitResultManifest(
			{
				attemptId: attemptNoTest.id,
				taskId: "task-no-test",
				baseCommit: "commit_0",
				candidateCommit: "commit_no_test",
				changedArtifacts: { "src/code.ts": "hash_code" },
				modifiedFiles: ["src/code.ts"],
				// testResults omitted!
			},
			attemptNoTest.epoch,
		);

		const passNoTest = await coordinator.verifyAndIntegrate(attemptNoTest.id);
		expect(passNoTest).toBe(false);
		expect(store.getTask("task-no-test")?.status).toBe("failed");

		// 2. Submit with failed test
		store.insertTask({
			id: "task-fail-test",
			goalId: "goal-clean-strict",
			title: "Task with failed test",
			dependencies: [],
			inputManifest: {},
			allowedPaths: ["src/code.ts"],
			retryLimit: 1,
			retryCount: 0,
			status: "ready",
		});

		const attemptFailTest = await coordinator.dispatchTask("task-fail-test", "worker-1", "commit_0", "");
		store.submitResultManifest(
			{
				attemptId: attemptFailTest.id,
				taskId: "task-fail-test",
				baseCommit: "commit_0",
				candidateCommit: "commit_fail_test",
				changedArtifacts: { "src/code.ts": "hash_code" },
				modifiedFiles: ["src/code.ts"],
				testResults: { passed: false, command: "npm test", outputHash: "fail_hash" },
			},
			attemptFailTest.epoch,
		);

		const passFailTest = await coordinator.verifyAndIntegrate(attemptFailTest.id);
		expect(passFailTest).toBe(false);
		expect(store.getTask("task-fail-test")?.status).toBe("failed");

		// 3. Submit without candidateCommit (cannot be integrated)
		store.insertTask({
			id: "task-no-commit",
			goalId: "goal-clean-strict",
			title: "Task without candidate commit",
			dependencies: [],
			inputManifest: {},
			allowedPaths: ["src/code.ts"],
			retryLimit: 1,
			retryCount: 0,
			status: "ready",
		});

		const attemptNoCommit = await coordinator.dispatchTask("task-no-commit", "worker-1", "commit_0", "");
		store.submitResultManifest(
			{
				attemptId: attemptNoCommit.id,
				taskId: "task-no-commit",
				baseCommit: "commit_0",
				// candidateCommit omitted!
				changedArtifacts: { "src/code.ts": "hash_code" },
				modifiedFiles: ["src/code.ts"],
				testResults: { passed: true, command: "npm test", outputHash: "pass_hash" },
			},
			attemptNoCommit.epoch,
		);

		const passNoCommit = await coordinator.verifyAndIntegrate(attemptNoCommit.id);
		expect(passNoCommit).toBe(false);
		expect(store.getTask("task-no-commit")?.status).toBe("failed");
	});

	test("Real Git Worktree Isolation, Execution, Test Verification and Cherry-Pick Integration", async () => {
		const gitRepoDir = path.join(tempDir, "git-repo");
		fs.mkdirSync(gitRepoDir, { recursive: true });
		execSync("git init -b main", { cwd: gitRepoDir });
		execSync("git config user.name 'DUME Tester'", { cwd: gitRepoDir });
		execSync("git config user.email 'tester@dum-e.local'", { cwd: gitRepoDir });
		fs.writeFileSync(path.join(gitRepoDir, "README.md"), "# Test Repo\n");
		execSync("git add README.md && git commit -m 'Initial commit'", { cwd: gitRepoDir });
		const baseCommit = execSync("git rev-parse HEAD", { cwd: gitRepoDir, encoding: "utf-8" }).trim();

		await coordinator.stop();
		const gitCoord = new DumeCoordinator(store, "coord-git-1", gitRepoDir);
		await gitCoord.start();

		store.insertGoal({
			id: "goal-git",
			title: "Real Git Goal",
			requirements: "Implement real git worktree change",
			requirementsRevision: 1,
			acceptanceCriteria: ["Must integrate cleanly"],
			status: "active",
			userInterrupted: false,
		});

		store.insertTask({
			id: "task-git-1",
			goalId: "goal-git",
			title: "Add feature file",
			dependencies: [],
			inputManifest: { testCommand: "node -e 'process.exit(0)'" },
			allowedPaths: ["feature.txt"],
			retryLimit: 1,
			retryCount: 0,
			status: "ready",
		});

		const worker = new DumeWorkerHost("worker-git", store, {
			repoRoot: gitRepoDir,
			worktreeBaseDir: path.join(tempDir, "worktrees"),
			agentRunner: async (wtPath) => {
				fs.writeFileSync(path.join(wtPath, "feature.txt"), "Real git worktree content\n");
			},
		});
		gitCoord.registerWorker(worker);

		const attempt = await gitCoord.dispatchTask("task-git-1", "worker-git", baseCommit, "");
		expect(attempt.baseCommit).toBe(baseCommit);

		const manifest = await gitCoord.executeAttempt(attempt.id);
		expect(manifest).not.toBeNull();
		expect(manifest?.modifiedFiles).toEqual(["feature.txt"]);
		expect(manifest?.candidateCommit).toBeDefined();
		expect(manifest?.testResults?.passed).toBe(true);

		// Verify task completed in store
		expect(store.getTask("task-git-1")?.status).toBe("completed");

		// Verify file actually merged/applied into main branch of gitRepoDir
		const mainLog = execSync("git log -n 1 --oneline", { cwd: gitRepoDir, encoding: "utf-8" });
		expect(mainLog).toContain("task-git-1");

		await gitCoord.stop();
	});

	test("Crash Recovery: Recover submitted results and supersede stale attempts across coordinator restarts", async () => {
		const gitRepoDir = path.join(tempDir, "git-repo-crash");
		fs.mkdirSync(gitRepoDir, { recursive: true });
		execSync("git init -b main", { cwd: gitRepoDir });
		execSync("git config user.name 'DUME Tester'", { cwd: gitRepoDir });
		execSync("git config user.email 'tester@dum-e.local'", { cwd: gitRepoDir });
		fs.writeFileSync(path.join(gitRepoDir, "README.md"), "# Crash Test Repo\n");
		execSync("git add README.md && git commit -m 'Initial commit'", { cwd: gitRepoDir });
		const baseCommit = execSync("git rev-parse HEAD", { cwd: gitRepoDir, encoding: "utf-8" }).trim();

		// Stop beforeEach coordinator so coord1 can acquire the primary lock
		await coordinator.stop();

		// Coordinator 1 starts
		const coord1 = new DumeCoordinator(store, "coord-crash-1", gitRepoDir);
		await coord1.start();

		store.insertGoal({
			id: "goal-crash",
			title: "Crash Recovery Goal",
			requirements: "Verify crash recovery",
			requirementsRevision: 1,
			acceptanceCriteria: ["Must recover submitted result"],
			status: "active",
			userInterrupted: false,
		});

		// Task A: will be submitted before crash
		store.insertTask({
			id: "task-crash-submitted",
			goalId: "goal-crash",
			title: "Task submitted before crash",
			dependencies: [],
			inputManifest: { testCommand: "node -e 'process.exit(0)'" },
			allowedPaths: ["recovered.txt"],
			retryLimit: 1,
			retryCount: 0,
			status: "ready",
		});

		// Task B: will be running when coordinator crashes
		store.insertTask({
			id: "task-crash-stale",
			goalId: "goal-crash",
			title: "Task in-flight during crash",
			dependencies: [],
			inputManifest: {},
			allowedPaths: ["stale.txt"],
			retryLimit: 1,
			retryCount: 0,
			status: "ready",
		});

		const attemptA = await coord1.dispatchTask("task-crash-submitted", "worker-1", baseCommit, "");
		const _attemptB = await coord1.dispatchTask("task-crash-stale", "worker-1", baseCommit, "");

		// Worker for Task A produces a commit on a branch
		execSync(`git checkout -b branch-a ${baseCommit}`, { cwd: gitRepoDir });
		fs.writeFileSync(path.join(gitRepoDir, "recovered.txt"), "Recovered successfully\n");
		execSync("git add recovered.txt && git commit -m 'feat: recover task-crash-submitted'", { cwd: gitRepoDir });
		const commitA = execSync("git rev-parse HEAD", { cwd: gitRepoDir, encoding: "utf-8" }).trim();
		execSync("git checkout main", { cwd: gitRepoDir });

		// Task A result is submitted to store, but NOT yet integrated
		store.submitResultManifest(
			{
				attemptId: attemptA.id,
				taskId: "task-crash-submitted",
				baseCommit,
				candidateCommit: commitA,
				changedArtifacts: { "recovered.txt": "hash_rec" },
				modifiedFiles: ["recovered.txt"],
				testResults: { passed: true, command: "test", outputHash: "test_ok" },
			},
			attemptA.epoch,
		);

		// Now simulate abrupt coordinator 1 crash (stop heartbeat and release lock)
		await coord1.stop();

		// Coordinator 2 starts up in a new epoch
		const coord2 = new DumeCoordinator(store, "coord-crash-2", gitRepoDir);
		await coord2.start();

		// Check recovery results:
		// 1. Task A should be verified and integrated by coord2 during recoverState()
		expect(store.getTask("task-crash-submitted")?.status).toBe("completed");
		const mainLog = execSync("git log -n 1 --oneline", { cwd: gitRepoDir, encoding: "utf-8" });
		expect(mainLog).toContain("recover task-crash-submitted");

		// 2. Task B (was running in superseded epoch 1) should be recovered and marked needs_attention
		expect(store.getTask("task-crash-stale")?.status).toBe("needs_attention");

		await coord2.stop();
	});
});
