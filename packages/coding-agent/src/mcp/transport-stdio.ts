/**
 * MCP Stdio Transport for DUM-E
 * Communicates with an MCP server subprocess via stdin/stdout line-delimited JSON-RPC 2.0.
 */

import { type ChildProcess, spawn } from "node:child_process";
import type {
	JsonRpcMessage,
	JsonRpcNotification,
	JsonRpcRequest,
	JsonRpcResponse,
	McpStdioServerConfig,
	McpTransport,
} from "./types.ts";

export class McpStdioTransport implements McpTransport {
	private config: McpStdioServerConfig;
	private process?: ChildProcess;
	private nextId = 1;
	private pendingRequests = new Map<
		string | number,
		{
			resolve: (res: unknown) => void;
			reject: (err: Error) => void;
		}
	>();
	private buffer = "";
	private closed = false;

	constructor(config: McpStdioServerConfig) {
		this.config = config;
	}

	async connect(): Promise<void> {
		if (this.process) return;

		const env = {
			...process.env,
			...(this.config.env ?? {}),
		};

		this.process = spawn(this.config.command, this.config.args ?? [], {
			cwd: this.config.cwd ?? process.cwd(),
			env,
			stdio: ["pipe", "pipe", "pipe"],
		});

		this.process.stdout?.setEncoding("utf-8");
		this.process.stderr?.setEncoding("utf-8");

		this.process.stdout?.on("data", (chunk: string) => {
			this.buffer += chunk;
			this.flushBuffer();
		});

		this.process.stderr?.on("data", (chunk: string) => {
			// MCP servers may write debug logs to stderr
			const trimmed = chunk.trim();
			if (trimmed) {
				// Avoid spamming test logs unless needed
			}
		});

		this.process.on("error", (err: Error) => {
			this.handleDisconnect(new Error(`MCP process failed: ${err.message}`));
		});

		this.process.on("exit", (code: number | null, signal: string | null) => {
			this.handleDisconnect(new Error(`MCP process exited with code ${code}, signal ${signal}`));
		});
	}

	private flushBuffer(): void {
		const lines = this.buffer.split("\n");
		// Keep remaining un-terminated line in buffer
		this.buffer = lines.pop() ?? "";

		for (const line of lines) {
			const trimmed = line.trim();
			if (!trimmed) continue;
			try {
				const message = JSON.parse(trimmed) as JsonRpcMessage;
				this.handleMessage(message);
			} catch {
				console.error("[MCP JSON Parse Error] Failed to parse message:", trimmed);
			}
		}
	}

	private handleMessage(message: JsonRpcMessage): void {
		if ("id" in message && message.id !== undefined) {
			const response = message as JsonRpcResponse;
			const pending = this.pendingRequests.get(response.id);
			if (pending) {
				this.pendingRequests.delete(response.id);
				if (response.error) {
					pending.reject(new Error(`MCP error ${response.error.code}: ${response.error.message}`));
				} else {
					pending.resolve(response.result);
				}
			}
		}
	}

	async sendRequest(method: string, params?: unknown, signal?: AbortSignal): Promise<unknown> {
		if (this.closed || !this.process?.stdin) {
			throw new Error("MCP Stdio Transport is not connected or already closed.");
		}

		if (signal?.aborted) {
			throw new Error("MCP Request was aborted before sending.");
		}

		const id = this.nextId++;
		const request: JsonRpcRequest = {
			jsonrpc: "2.0",
			id,
			method,
			params,
		};

		return new Promise((resolve, reject) => {
			let abortListener: (() => void) | undefined;

			if (signal) {
				abortListener = () => {
					this.sendNotification("notifications/cancelled", {
						requestId: id,
						reason: "User cancelled request",
					}).catch(() => {});

					this.pendingRequests.delete(id);
					reject(new Error(`MCP Request ${method} (id ${id}) was aborted.`));
				};
				signal.addEventListener("abort", abortListener, { once: true });
			}

			this.pendingRequests.set(id, {
				resolve: (result) => {
					if (abortListener && signal) {
						signal.removeEventListener("abort", abortListener);
					}
					resolve(result);
				},
				reject: (err) => {
					if (abortListener && signal) {
						signal.removeEventListener("abort", abortListener);
					}
					reject(err);
				},
			});

			try {
				this.process?.stdin?.write(`${JSON.stringify(request)}\n`);
			} catch (writeErr: any) {
				this.pendingRequests.delete(id);
				reject(new Error(`Failed to write MCP request: ${writeErr.message}`));
			}
		});
	}

	async sendNotification(method: string, params?: unknown): Promise<void> {
		if (this.closed || !this.process?.stdin) {
			return;
		}

		const notif: JsonRpcNotification = {
			jsonrpc: "2.0",
			method,
			params,
		};

		try {
			this.process.stdin.write(`${JSON.stringify(notif)}\n`);
		} catch {
			// Notifications are best effort
		}
	}

	private handleDisconnect(err: Error): void {
		if (this.closed) return;
		for (const [, pending] of this.pendingRequests) {
			pending.reject(err);
		}
		this.pendingRequests.clear();
	}

	async close(): Promise<void> {
		if (this.closed) return;
		this.closed = true;

		this.handleDisconnect(new Error("MCP Transport closed"));

		if (this.process) {
			try {
				this.process.stdin?.end();
				this.process.kill("SIGTERM");

				// Grace period
				const proc = this.process;
				await new Promise<void>((resolve) => {
					const timer = setTimeout(() => {
						try {
							proc.kill("SIGKILL");
						} catch {}
						resolve();
					}, 1000);

					proc.on("exit", () => {
						clearTimeout(timer);
						resolve();
					});
				});
			} catch {
				// Ignore cleanup errors
			}
			this.process = undefined;
		}
	}
}
