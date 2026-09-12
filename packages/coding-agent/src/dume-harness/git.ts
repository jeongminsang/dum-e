/**
 * DUM-E Harness Git Operations
 * Real Git worktree management, commit creation, diff inspection, and integration.
 * Conforms to HARNESS-DESIGN.md §3, §6, §8
 */

import { execFile } from "node:child_process";
import * as fs from "node:fs";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

export interface GitExecResult {
	stdout: string;
	stderr: string;
	exitCode: number;
}

export async function runGit(args: string[], cwd: string): Promise<GitExecResult> {
	try {
		const { stdout, stderr } = await execFileAsync("git", args, {
			cwd,
			encoding: "utf-8",
			maxBuffer: 10 * 1024 * 1024,
		});
		return { stdout: stdout.trim(), stderr: stderr.trim(), exitCode: 0 };
	} catch (err: any) {
		return {
			stdout: err.stdout ? String(err.stdout).trim() : "",
			stderr: err.stderr ? String(err.stderr).trim() : err.message || String(err),
			exitCode: typeof err.code === "number" ? err.code : 1,
		};
	}
}

export async function getRepoRoot(cwd: string): Promise<string | null> {
	const res = await runGit(["rev-parse", "--show-toplevel"], cwd);
	if (res.exitCode !== 0) return null;
	return res.stdout;
}

export async function getHeadCommit(cwd: string): Promise<string> {
	const res = await runGit(["rev-parse", "HEAD"], cwd);
	if (res.exitCode !== 0) {
		throw new Error(`Failed to get HEAD commit in ${cwd}: ${res.stderr}`);
	}
	return res.stdout;
}

export async function createGitWorktree(repoRoot: string, worktreePath: string, baseCommit: string): Promise<void> {
	if (fs.existsSync(worktreePath)) {
		throw new Error(`Worktree destination path already exists: ${worktreePath}`);
	}

	const res = await runGit(["worktree", "add", "--detach", worktreePath, baseCommit], repoRoot);
	if (res.exitCode !== 0) {
		throw new Error(`Failed to create git worktree at ${worktreePath}: ${res.stderr}`);
	}
}

export async function removeGitWorktree(repoRoot: string, worktreePath: string): Promise<void> {
	if (fs.existsSync(worktreePath)) {
		const res = await runGit(["worktree", "remove", "--force", worktreePath], repoRoot);
		if (res.exitCode !== 0) {
			// If git worktree remove fails, attempt manual cleanup and prune
			try {
				fs.rmSync(worktreePath, { recursive: true, force: true });
			} catch {
				// ignore filesystem rm errors
			}
		}
	}
	await runGit(["worktree", "prune"], repoRoot);
}

export async function getWorktreeModifiedFiles(worktreePath: string): Promise<string[]> {
	const res = await runGit(["status", "--porcelain=v1"], worktreePath);
	if (res.exitCode !== 0) {
		throw new Error(`Failed to get git status in ${worktreePath}: ${res.stderr}`);
	}

	const files: string[] = [];
	for (const line of res.stdout.split("\n")) {
		const trimmed = line.trim();
		if (!trimmed) continue;
		// Porcelain v1 format: XY PATH (or XY PATH -> NEW_PATH)
		const filePath = trimmed.slice(3).trim();
		if (filePath) {
			files.push(filePath.includes(" -> ") ? filePath.split(" -> ")[1] : filePath);
		}
	}
	return files;
}

export async function commitWorktreeChanges(worktreePath: string, commitMessage: string): Promise<string> {
	const addRes = await runGit(["add", "-A"], worktreePath);
	if (addRes.exitCode !== 0) {
		throw new Error(`git add failed in worktree ${worktreePath}: ${addRes.stderr}`);
	}

	const commitRes = await runGit(["commit", "-m", commitMessage, "--allow-empty"], worktreePath);
	if (commitRes.exitCode !== 0) {
		throw new Error(`git commit failed in worktree ${worktreePath}: ${commitRes.stderr}`);
	}

	return await getHeadCommit(worktreePath);
}

export async function integrateCandidateCommit(
	repoRoot: string,
	_targetBranch: string,
	candidateCommit: string,
): Promise<{ success: boolean; error?: string; conflict?: boolean }> {
	// 1. Verify candidateCommit exists in the git object database
	const catRes = await runGit(["cat-file", "-t", candidateCommit], repoRoot);
	if (catRes.exitCode !== 0 || catRes.stdout !== "commit") {
		return {
			success: false,
			error: `Candidate commit ${candidateCommit} is not a valid commit object in ${repoRoot}`,
		};
	}

	// 2. Try cherry-picking into target branch or testing merge-base
	const cherryRes = await runGit(["cherry-pick", candidateCommit], repoRoot);
	if (cherryRes.exitCode !== 0) {
		// Abort cherry-pick to prevent dirty working directory
		await runGit(["cherry-pick", "--abort"], repoRoot);
		return {
			success: false,
			conflict: true,
			error: `Cherry-pick of candidate commit ${candidateCommit} failed: ${cherryRes.stderr}`,
		};
	}

	return { success: true };
}
