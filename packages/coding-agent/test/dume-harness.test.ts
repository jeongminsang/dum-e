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
		coordinator = new DumeCoordinator(store, "coord-pi-1");
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

		// Valid submit
		const validSubmit = store.submitResultManifest(
			{
				attemptId: attempt1.id,
				taskId: "task-1",
				baseCommit: "commit_0",
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
				changedArtifacts: { "/etc/shadow": "bad_hash" },
				modifiedFiles: ["/etc/shadow"],
			},
			attempt.epoch,
		);

		const pass = await coordinator.verifyAndIntegrate(attempt.id);
		expect(pass).toBe(false);
		expect(store.getTask("task-sec")?.status).toBe("failed");
	});
});
