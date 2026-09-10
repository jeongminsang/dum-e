/**
 * DUM-E Harness Core Contracts and Type Definitions
 * Based on HARNESS-DESIGN.md & DUM-E-IMPLEMENTATION.md
 */

export type GoalStatus =
	| "pending"
	| "active"
	| "verifying"
	| "completed"
	| "cancelled"
	| "needs_attention";

export type TaskStatus =
	| "blocked"
	| "ready"
	| "running"
	| "verifying"
	| "integrating"
	| "completed"
	| "needs_attention"
	| "failed"
	| "cancelled";

export type AttemptStatus =
	| "allocated"
	| "starting"
	| "running"
	| "result_submitted"
	| "accepted"
	| "rejected"
	| "cancel_requested"
	| "lost"
	| "outcome_unknown"
	| "cancelled";

export type ExternalOperationStatus =
	| "intent_recorded"
	| "dispatched"
	| "confirmed"
	| "failed"
	| "outcome_unknown";

export interface GoalRecord {
	id: string;
	title: string;
	requirements: string;
	requirementsRevision: number;
	acceptanceCriteria: string[];
	status: GoalStatus;
	userInterrupted: boolean;
	createdAt: number;
	updatedAt: number;
	completedAt?: number;
}

export interface TaskRecord {
	id: string;
	goalId: string;
	title: string;
	dependencies: string[]; // taskIds
	inputManifest: Record<string, string>; // file/artifact path -> hash
	allowedPaths: string[]; // Glob / path prefixes allowed to be modified
	retryLimit: number;
	retryCount: number;
	status: TaskStatus;
	activeAttemptId?: string;
	createdAt: number;
	updatedAt: number;
}

export interface AttemptRecord {
	id: string;
	taskId: string;
	epoch: number;
	coordinatorEpoch: number;
	workerId: string;
	leaseExpiresAt: number;
	baseCommit: string;
	worktreePath: string;
	status: AttemptStatus;
	createdAt: number;
	updatedAt: number;
	heartbeatAt: number;
}

export interface ResultManifest {
	attemptId: string;
	taskId: string;
	baseCommit: string;
	changedArtifacts: Record<string, string>; // path -> content SHA-256
	modifiedFiles: string[];
	executionLogsHash?: string;
	remainingIssues?: string[];
	testResults?: {
		passed: boolean;
		command: string;
		outputHash: string;
	};
}

export interface VerificationRecord {
	id: string;
	taskId: string;
	attemptId: string;
	manifestHash: string;
	requirementsRevision: number;
	command: string;
	passed: boolean;
	logsHash: string;
	verifiedAt: number;
}

export interface IntegrationRecord {
	id: string;
	goalId: string;
	taskId: string;
	targetBranch: string;
	baseCommit: string;
	candidateCommit: string;
	status: "pending" | "applied" | "conflict" | "failed";
	conflictDetails?: string;
	integratedAt?: number;
}

export interface ExternalOperationRecord {
	id: string;
	taskId: string;
	intent: string;
	idempotencyKey: string;
	status: ExternalOperationStatus;
	checkStrategy: "query_receipt" | "read_only" | "manual";
	receiptData?: string;
	createdAt: number;
	updatedAt: number;
}

export interface HarnessEventRecord {
	id: number;
	entityId: string;
	eventType: string;
	epoch: number;
	payload: string;
	dedupKey?: string;
	createdAt: number;
}
