/**
 * DUM-E Harness SQLite Storage
 * Implements transactionally safe persistence for Goals, Tasks, Attempts, Events, Artifacts, and ExternalOperations.
 * Conforms to HARNESS-DESIGN.md §4 & §5
 */

import { Database } from "bun:sqlite";
import * as fs from "node:fs";
import * as path from "node:path";
import * as crypto from "node:crypto";
import type {
	GoalRecord,
	TaskRecord,
	AttemptRecord,
	VerificationRecord,
	IntegrationRecord,
	ExternalOperationRecord,
	HarnessEventRecord,
	ResultManifest,
} from "./types";

export class HarnessStore {
	private db: Database;
	private artifactDir: string;

	constructor(dbPath: string, artifactDir?: string) {
		const dir = path.dirname(dbPath);
		if (!fs.existsSync(dir)) {
			fs.mkdirSync(dir, { recursive: true });
		}
		this.artifactDir = artifactDir ?? path.join(dir, "artifacts");
		if (!fs.existsSync(this.artifactDir)) {
			fs.mkdirSync(this.artifactDir, { recursive: true });
		}

		this.db = new Database(dbPath);
		this.db.exec("PRAGMA journal_mode = WAL;");
		this.db.exec("PRAGMA synchronous = NORMAL;");
		this.db.exec("PRAGMA foreign_keys = ON;");
		this.initSchema();
	}

	private initSchema(): void {
		this.db.exec(`
			CREATE TABLE IF NOT EXISTS goals (
				id TEXT PRIMARY KEY,
				title TEXT NOT NULL,
				requirements TEXT NOT NULL,
				requirementsRevision INTEGER NOT NULL DEFAULT 1,
				acceptanceCriteria TEXT NOT NULL,
				status TEXT NOT NULL,
				userInterrupted INTEGER NOT NULL DEFAULT 0,
				createdAt INTEGER NOT NULL,
				updatedAt INTEGER NOT NULL,
				completedAt INTEGER
			);

			CREATE TABLE IF NOT EXISTS tasks (
				id TEXT PRIMARY KEY,
				goalId TEXT NOT NULL,
				title TEXT NOT NULL,
				dependencies TEXT NOT NULL,
				inputManifest TEXT NOT NULL,
				allowedPaths TEXT NOT NULL,
				retryLimit INTEGER NOT NULL DEFAULT 3,
				retryCount INTEGER NOT NULL DEFAULT 0,
				status TEXT NOT NULL,
				activeAttemptId TEXT,
				createdAt INTEGER NOT NULL,
				updatedAt INTEGER NOT NULL,
				FOREIGN KEY (goalId) REFERENCES goals(id) ON DELETE CASCADE
			);

			CREATE TABLE IF NOT EXISTS attempts (
				id TEXT PRIMARY KEY,
				taskId TEXT NOT NULL,
				epoch INTEGER NOT NULL,
				coordinatorEpoch INTEGER NOT NULL,
				workerId TEXT NOT NULL,
				leaseExpiresAt INTEGER NOT NULL,
				baseCommit TEXT NOT NULL,
				worktreePath TEXT NOT NULL,
				status TEXT NOT NULL,
				createdAt INTEGER NOT NULL,
				updatedAt INTEGER NOT NULL,
				heartbeatAt INTEGER NOT NULL,
				FOREIGN KEY (taskId) REFERENCES tasks(id) ON DELETE CASCADE
			);

			CREATE TABLE IF NOT EXISTS result_manifests (
				attemptId TEXT PRIMARY KEY,
				taskId TEXT NOT NULL,
				baseCommit TEXT NOT NULL,
				manifestJson TEXT NOT NULL,
				manifestHash TEXT NOT NULL,
				createdAt INTEGER NOT NULL,
				FOREIGN KEY (attemptId) REFERENCES attempts(id) ON DELETE CASCADE
			);

			CREATE TABLE IF NOT EXISTS verifications (
				id TEXT PRIMARY KEY,
				taskId TEXT NOT NULL,
				attemptId TEXT NOT NULL,
				manifestHash TEXT NOT NULL,
				requirementsRevision INTEGER NOT NULL,
				command TEXT NOT NULL,
				passed INTEGER NOT NULL,
				logsHash TEXT NOT NULL,
				verifiedAt INTEGER NOT NULL
			);

			CREATE TABLE IF NOT EXISTS integrations (
				id TEXT PRIMARY KEY,
				goalId TEXT NOT NULL,
				taskId TEXT NOT NULL,
				targetBranch TEXT NOT NULL,
				baseCommit TEXT NOT NULL,
				candidateCommit TEXT NOT NULL,
				status TEXT NOT NULL,
				conflictDetails TEXT,
				integratedAt INTEGER
			);

			CREATE TABLE IF NOT EXISTS external_operations (
				id TEXT PRIMARY KEY,
				taskId TEXT NOT NULL,
				intent TEXT NOT NULL,
				idempotencyKey TEXT NOT NULL UNIQUE,
				status TEXT NOT NULL,
				checkStrategy TEXT NOT NULL,
				receiptData TEXT,
				createdAt INTEGER NOT NULL,
				updatedAt INTEGER NOT NULL
			);

			CREATE TABLE IF NOT EXISTS coordinator_locks (
				id TEXT PRIMARY KEY,
				ownerId TEXT NOT NULL,
				epoch INTEGER NOT NULL,
				heartbeatAt INTEGER NOT NULL
			);

			CREATE TABLE IF NOT EXISTS harness_events (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				entityId TEXT NOT NULL,
				eventType TEXT NOT NULL,
				epoch INTEGER NOT NULL,
				payload TEXT NOT NULL,
				dedupKey TEXT UNIQUE,
				createdAt INTEGER NOT NULL
			);
		`);
	}

	acquireCoordinatorLock(ownerId: string, ttlMs: number = 10000): { acquired: boolean; epoch: number } {
		const now = Date.now();
		const current = this.db
			.query("SELECT ownerId, epoch, heartbeatAt FROM coordinator_locks WHERE id = 'primary'")
			.get() as { ownerId: string; epoch: number; heartbeatAt: number } | null;

		if (!current) {
			this.db
				.query("INSERT INTO coordinator_locks (id, ownerId, epoch, heartbeatAt) VALUES ('primary', ?, 1, ?)")
				.run(ownerId, now);
			return { acquired: true, epoch: 1 };
		}

		if (current.ownerId === ownerId) {
			this.db
				.query("UPDATE coordinator_locks SET heartbeatAt = ? WHERE id = 'primary'")
				.run(now);
			return { acquired: true, epoch: current.epoch };
		}

		if (now - current.heartbeatAt > ttlMs) {
			const nextEpoch = current.epoch + 1;
			this.db
				.query("UPDATE coordinator_locks SET ownerId = ?, epoch = ?, heartbeatAt = ? WHERE id = 'primary'")
				.run(ownerId, nextEpoch, now);
			return { acquired: true, epoch: nextEpoch };
		}

		return { acquired: false, epoch: current.epoch };
	}

	heartbeatCoordinatorLock(ownerId: string, epoch: number): boolean {
		const res = this.db
			.query("UPDATE coordinator_locks SET heartbeatAt = ? WHERE id = 'primary' AND ownerId = ? AND epoch = ?")
			.run(Date.now(), ownerId, epoch);
		return res.changes > 0;
	}

	releaseCoordinatorLock(ownerId: string, epoch: number): void {
		this.db
			.query("DELETE FROM coordinator_locks WHERE id = 'primary' AND ownerId = ? AND epoch = ?")
			.run(ownerId, epoch);
	}

	insertGoal(goal: Omit<GoalRecord, "createdAt" | "updatedAt">): GoalRecord {
		const now = Date.now();
		const record: GoalRecord = {
			...goal,
			createdAt: now,
			updatedAt: now,
		};
		this.db
			.query(
				`INSERT INTO goals (id, title, requirements, requirementsRevision, acceptanceCriteria, status, userInterrupted, createdAt, updatedAt, completedAt)
				VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`
			)
			.run(
				record.id,
				record.title,
				record.requirements,
				record.requirementsRevision,
				JSON.stringify(record.acceptanceCriteria),
				record.status,
				record.userInterrupted ? 1 : 0,
				record.createdAt,
				record.updatedAt,
				record.completedAt ?? null
			);

		this.recordEvent(record.id, "goal_created", 1, record);
		return record;
	}

	getGoal(id: string): GoalRecord | null {
		const row = this.db.query("SELECT * FROM goals WHERE id = ?").get(id) as any;
		if (!row) return null;
		return {
			...row,
			acceptanceCriteria: JSON.parse(row.acceptanceCriteria),
			userInterrupted: Boolean(row.userInterrupted),
		};
	}

	updateGoalStatus(id: string, status: GoalRecord["status"], completedAt?: number): void {
		const now = Date.now();
		this.db
			.query("UPDATE goals SET status = ?, updatedAt = ?, completedAt = COALESCE(?, completedAt) WHERE id = ?")
			.run(status, now, completedAt ?? null, id);
		this.recordEvent(id, "goal_status_changed", 0, { status, completedAt });
	}

	setUserInterrupted(id: string, interrupted: boolean): void {
		this.db
			.query("UPDATE goals SET userInterrupted = ?, updatedAt = ? WHERE id = ?")
			.run(interrupted ? 1 : 0, Date.now(), id);
		this.recordEvent(id, "goal_interrupted", 0, { userInterrupted: interrupted });
	}

	insertTask(task: Omit<TaskRecord, "createdAt" | "updatedAt">): TaskRecord {
		const now = Date.now();
		const record: TaskRecord = {
			...task,
			createdAt: now,
			updatedAt: now,
		};
		this.db
			.query(
				`INSERT INTO tasks (id, goalId, title, dependencies, inputManifest, allowedPaths, retryLimit, retryCount, status, activeAttemptId, createdAt, updatedAt)
				VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`
			)
			.run(
				record.id,
				record.goalId,
				record.title,
				JSON.stringify(record.dependencies),
				JSON.stringify(record.inputManifest),
				JSON.stringify(record.allowedPaths),
				record.retryLimit,
				record.retryCount,
				record.status,
				record.activeAttemptId ?? null,
				record.createdAt,
				record.updatedAt
			);

		this.recordEvent(record.id, "task_created", 1, record);
		return record;
	}

	getTask(id: string): TaskRecord | null {
		const row = this.db.query("SELECT * FROM tasks WHERE id = ?").get(id) as any;
		if (!row) return null;
		return {
			...row,
			dependencies: JSON.parse(row.dependencies),
			inputManifest: JSON.parse(row.inputManifest),
			allowedPaths: JSON.parse(row.allowedPaths),
		};
	}

	getTasksByGoal(goalId: string): TaskRecord[] {
		const rows = this.db.query("SELECT * FROM tasks WHERE goalId = ?").all(goalId) as any[];
		return rows.map((row) => ({
			...row,
			dependencies: JSON.parse(row.dependencies),
			inputManifest: JSON.parse(row.inputManifest),
			allowedPaths: JSON.parse(row.allowedPaths),
		}));
	}

	updateTaskStatus(id: string, status: TaskRecord["status"], activeAttemptId?: string): void {
		const now = Date.now();
		this.db
			.query("UPDATE tasks SET status = ?, activeAttemptId = COALESCE(?, activeAttemptId), updatedAt = ? WHERE id = ?")
			.run(status, activeAttemptId ?? null, now, id);
		this.recordEvent(id, "task_status_changed", 0, { status, activeAttemptId });
	}

	createAttempt(
		taskId: string,
		coordinatorEpoch: number,
		workerId: string,
		baseCommit: string,
		worktreePath: string,
		ttlMs: number = 30000
	): AttemptRecord {
		return this.db.transaction(() => {
			const task = this.getTask(taskId);
			if (!task) throw new Error(`Task ${taskId} not found`);

			const maxEpochRow = this.db
				.query("SELECT MAX(epoch) as maxEpoch FROM attempts WHERE taskId = ?")
				.get(taskId) as { maxEpoch: number | null };
			const nextEpoch = (maxEpochRow?.maxEpoch ?? 0) + 1;

			const attemptId = `att_${crypto.randomUUID().slice(0, 12)}`;
			const now = Date.now();
			const record: AttemptRecord = {
				id: attemptId,
				taskId,
				epoch: nextEpoch,
				coordinatorEpoch,
				workerId,
				leaseExpiresAt: now + ttlMs,
				baseCommit,
				worktreePath,
				status: "allocated",
				createdAt: now,
				updatedAt: now,
				heartbeatAt: now,
			};

			this.db
				.query(
					`INSERT INTO attempts (id, taskId, epoch, coordinatorEpoch, workerId, leaseExpiresAt, baseCommit, worktreePath, status, createdAt, updatedAt, heartbeatAt)
					VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`
				)
				.run(
					record.id,
					record.taskId,
					record.epoch,
					record.coordinatorEpoch,
					record.workerId,
					record.leaseExpiresAt,
					record.baseCommit,
					record.worktreePath,
					record.status,
					record.createdAt,
					record.updatedAt,
					record.heartbeatAt
				);

			this.db
				.query("UPDATE tasks SET activeAttemptId = ?, status = 'running', updatedAt = ? WHERE id = ?")
				.run(record.id, now, taskId);

			this.recordEvent(record.id, "attempt_allocated", nextEpoch, record);
			return record;
		})();
	}

	getAttempt(id: string): AttemptRecord | null {
		return (this.db.query("SELECT * FROM attempts WHERE id = ?").get(id) as AttemptRecord) ?? null;
	}

	heartbeatAttempt(attemptId: string, epoch: number, extendMs: number = 30000): boolean {
		const now = Date.now();
		const res = this.db
			.query(
				"UPDATE attempts SET heartbeatAt = ?, leaseExpiresAt = ?, updatedAt = ? WHERE id = ? AND epoch = ? AND status IN ('starting', 'running')"
			)
			.run(now, now + extendMs, now, attemptId, epoch);
		return res.changes > 0;
	}

	submitResultManifest(manifest: ResultManifest, attemptEpoch: number): { accepted: boolean; reason?: string } {
		return this.db.transaction(() => {
			const attempt = this.getAttempt(manifest.attemptId);
			if (!attempt) return { accepted: false, reason: "Attempt not found" };

			if (attempt.epoch !== attemptEpoch) {
				return {
					accepted: false,
					reason: `Epoch mismatch: expected ${attempt.epoch}, received ${attemptEpoch} (Fencing Guard)`,
				};
			}

			const task = this.getTask(manifest.taskId);
			if (!task || task.activeAttemptId !== manifest.attemptId) {
				return {
					accepted: false,
					reason: `Attempt ${manifest.attemptId} is no longer active for task ${manifest.taskId}`,
				};
			}

			const json = JSON.stringify(manifest);
			const hash = crypto.createHash("sha256").update(json).digest("hex");
			const now = Date.now();

			this.db
				.query(
					`INSERT OR REPLACE INTO result_manifests (attemptId, taskId, baseCommit, manifestJson, manifestHash, createdAt)
					VALUES (?, ?, ?, ?, ?, ?)`
				)
				.run(manifest.attemptId, manifest.taskId, manifest.baseCommit, json, hash, now);

			this.db
				.query("UPDATE attempts SET status = 'result_submitted', updatedAt = ? WHERE id = ?")
				.run(now, manifest.attemptId);

			this.db
				.query("UPDATE tasks SET status = 'verifying', updatedAt = ? WHERE id = ?")
				.run(now, manifest.taskId);

			this.recordEvent(manifest.attemptId, "result_submitted", attemptEpoch, {
				manifestHash: hash,
				modifiedFiles: manifest.modifiedFiles,
			});

			return { accepted: true };
		})();
	}

	getResultManifest(attemptId: string): ResultManifest | null {
		const row = this.db.query("SELECT manifestJson FROM result_manifests WHERE attemptId = ?").get(attemptId) as any;
		return row ? JSON.parse(row.manifestJson) : null;
	}

	saveArtifact(content: string | Buffer): { hash: string; size: number } {
		const buffer = Buffer.isBuffer(content) ? content : Buffer.from(content, "utf-8");
		const hash = crypto.createHash("sha256").update(buffer).digest("hex");
		const subDir = path.join(this.artifactDir, hash.slice(0, 2));
		if (!fs.existsSync(subDir)) fs.mkdirSync(subDir, { recursive: true });
		const filePath = path.join(subDir, hash);
		if (!fs.existsSync(filePath)) {
			fs.writeFileSync(filePath, buffer);
		}
		return { hash, size: buffer.length };
	}

	readArtifact(hash: string): Buffer | null {
		const filePath = path.join(this.artifactDir, hash.slice(0, 2), hash);
		if (!fs.existsSync(filePath)) return null;
		return fs.readFileSync(filePath);
	}

	recordVerification(verification: VerificationRecord): void {
		this.db
			.query(
				`INSERT INTO verifications (id, taskId, attemptId, manifestHash, requirementsRevision, command, passed, logsHash, verifiedAt)
				VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`
			)
			.run(
				verification.id,
				verification.taskId,
				verification.attemptId,
				verification.manifestHash,
				verification.requirementsRevision,
				verification.command,
				verification.passed ? 1 : 0,
				verification.logsHash,
				verification.verifiedAt
			);
	}

	recordIntegration(integration: IntegrationRecord): void {
		this.db
			.query(
				`INSERT OR REPLACE INTO integrations (id, goalId, taskId, targetBranch, baseCommit, candidateCommit, status, conflictDetails, integratedAt)
				VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`
			)
			.run(
				integration.id,
				integration.goalId,
				integration.taskId,
				integration.targetBranch,
				integration.baseCommit,
				integration.candidateCommit,
				integration.status,
				integration.conflictDetails ?? null,
				integration.integratedAt ?? null
			);
	}

	getUnfinishedAttempts(): AttemptRecord[] {
		return this.db
			.query("SELECT * FROM attempts WHERE status IN ('allocated', 'starting', 'running', 'result_submitted')")
			.all() as AttemptRecord[];
	}

	private recordEvent(entityId: string, eventType: string, epoch: number, payload: any): void {
		const now = Date.now();
		this.db
			.query(
				"INSERT INTO harness_events (entityId, eventType, epoch, payload, createdAt) VALUES (?, ?, ?, ?, ?)"
			)
			.run(entityId, eventType, epoch, JSON.stringify(payload), now);
	}

	close(): void {
		this.db.close();
	}
}
