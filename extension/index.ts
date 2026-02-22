/**
 * pup — Pi extension
 *
 * Exposes pi session state and streaming events over a Unix domain socket.
 * The daemon connects to this socket to bridge sessions to chat platforms.
 *
 * Socket protocol: newline-delimited JSON over Unix socket at ~/.pi/pup/<session-id>.sock
 * Multiple clients supported. Backend-agnostic — knows nothing about Telegram.
 */

import type { ExtensionAPI, ExtensionContext } from "@mariozechner/pi-coding-agent";
import * as net from "node:net";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import * as crypto from "node:crypto";

const SOCKET_DIR = process.env.PUP_SOCKET_DIR || path.join(os.homedir(), ".pi", "pup");
const DEFAULT_HISTORY_TURNS = 5;
const NAME_POLL_INTERVAL = 1000;
const SOCKET_CHECK_INTERVAL = 2000;

// Stable identifier for this pi process. Persists across /new and /compact
// so the daemon keeps the same topic for the lifetime of the process.
const INSTANCE_ID = crypto.randomUUID();

export default function (pi: ExtensionAPI) {
	let server: net.Server | null = null;
	let clients: Set<net.Socket> = new Set();
	let socketPath: string | null = null;
	let aliasPath: string | null = null;
	let sessionId: string | null = null;
	let currentName: string | undefined;
	let namePollTimer: ReturnType<typeof setInterval> | null = null;
	let socketCheckTimer: ReturnType<typeof setInterval> | null = null;
	let savedCtx: ExtensionContext | null = null;

	// ── Echo suppression ────────────────────────────────────────
	// Track messages we sent via IPC so we can tag their input events as echoes.
	const pendingSends: Set<string> = new Set();

	function normalizeText(text: string): string {
		return text.trim().toLowerCase();
	}

	// ── Streaming state ─────────────────────────────────────────
	// Accumulate partial text while the assistant is streaming, so clients
	// connecting mid-stream can receive the current state.
	let isStreaming = false;
	let currentMessageId: string | null = null;
	let accumulatedText = "";
	let accumulatedThinking = "";

	// ── Client management ───────────────────────────────────────

	function broadcastEvent(event: string, data: Record<string, unknown> = {}) {
		const msg = JSON.stringify({ type: "event", event, data }) + "\n";
		for (const client of clients) {
			try {
				client.write(msg);
			} catch {
				// Client disconnected; will be cleaned up on close.
			}
		}
	}

	function sendToClient(client: net.Socket, event: string, data: Record<string, unknown> = {}) {
		const msg = JSON.stringify({ type: "event", event, data }) + "\n";
		try {
			client.write(msg);
		} catch {
			// ignore
		}
	}

	function sendResponse(
		client: net.Socket,
		command: string,
		id: string | undefined,
		success: boolean,
		data?: unknown,
		error?: string,
	) {
		const msg = JSON.stringify({
			type: "response",
			command,
			...(id !== undefined ? { id } : {}),
			success,
			...(data !== undefined ? { data } : {}),
			...(error !== undefined ? { error } : {}),
		}) + "\n";
		try {
			client.write(msg);
		} catch {
			// ignore
		}
	}

	// ── History reconstruction ──────────────────────────────────

	interface Turn {
		user: { content: string; timestamp: number } | null;
		assistant: { content: string; timestamp: number } | null;
		tool_calls: {
			tool_call_id: string;
			tool_name: string;
			args: Record<string, unknown>;
			content: string;
			is_error: boolean;
		}[];
	}

	function getHistory(ctx: ExtensionContext, maxTurns: number = DEFAULT_HISTORY_TURNS): Turn[] {
		const branch = ctx.sessionManager.getBranch();
		const turns: Turn[] = [];
		let currentTurn: Turn | null = null;

		for (const entry of branch) {
			if (entry.type !== "message") continue;

			const msg = entry.message;
			if (msg.role === "user") {
				// Start a new turn.
				if (currentTurn) {
					turns.push(currentTurn);
				}
				const textContent = Array.isArray(msg.content)
					? msg.content
							.filter((c: any) => c.type === "text")
							.map((c: any) => c.text)
							.join("")
					: typeof msg.content === "string"
						? msg.content
						: "";
				currentTurn = {
					user: { content: textContent, timestamp: entry.timestamp ?? Date.now() },
					assistant: null,
					tool_calls: [],
				};
			} else if (msg.role === "assistant") {
				if (!currentTurn) {
					currentTurn = { user: null, assistant: null, tool_calls: [] };
				}
				const textContent = Array.isArray(msg.content)
					? msg.content
							.filter((c: any) => c.type === "text")
							.map((c: any) => c.text)
							.join("")
					: typeof msg.content === "string"
						? msg.content
						: "";
				currentTurn.assistant = {
					content: textContent,
					timestamp: entry.timestamp ?? Date.now(),
				};
			} else if (msg.role === "toolResult") {
				if (currentTurn) {
					const textContent = Array.isArray(msg.content)
						? msg.content
								.filter((c: any) => c.type === "text")
								.map((c: any) => c.text)
								.join("")
						: typeof msg.content === "string"
							? msg.content
							: "";
					currentTurn.tool_calls.push({
						tool_call_id: (msg as any).toolCallId ?? "",
						tool_name: (msg as any).toolName ?? "",
						args: {},
						content: textContent,
						is_error: (msg as any).isError ?? false,
					});
				}
			}
		}

		if (currentTurn) {
			turns.push(currentTurn);
		}

		// Return last N turns.
		return turns.slice(-maxTurns);
	}

	// ── Socket server setup ─────────────────────────────────────

	function ensureSocketDir() {
		fs.mkdirSync(SOCKET_DIR, { recursive: true, mode: 0o700 });
	}

	function createSocketServer(ctx: ExtensionContext) {
		ensureSocketDir();

		savedCtx = ctx;
		sessionId = ctx.sessionManager.getSessionId() ?? null;
		socketPath = path.join(SOCKET_DIR, `${INSTANCE_ID}.sock`);

		// Clean up stale socket.
		try {
			fs.unlinkSync(socketPath);
		} catch {
			// ignore
		}

		server = net.createServer((client) => {
			clients.add(client);

			// Send hello + history on connect.
			// Use savedCtx (not a captured ctx) so clients connecting after
			// a /new or /compact always get the latest session state.
			const ctx = savedCtx!;
			const helloData: Record<string, unknown> = {
				session_id: INSTANCE_ID,
				cwd: ctx.cwd,
			};

			const name = pi.getSessionName();
			if (name) helloData.session_name = name;

			const sessionFile = ctx.sessionManager.getSessionFile();
			if (sessionFile) helloData.session_file = sessionFile;

			helloData.thinking_level = pi.getThinkingLevel();

			sendToClient(client, "hello", helloData);

			// Send history.
			const turns = getHistory(ctx);
			sendToClient(client, "history", {
				turns,
				streaming: isStreaming,
				...(isStreaming && accumulatedText ? { partial_text: accumulatedText } : {}),
			});

			// Handle incoming commands.
			let buffer = "";
			client.on("data", (data) => {
				buffer += data.toString();
				const lines = buffer.split("\n");
				buffer = lines.pop() ?? "";

				for (const line of lines) {
					if (!line.trim()) continue;
					try {
						const msg = JSON.parse(line);
						handleCommand(client, msg, savedCtx!);
					} catch {
						sendResponse(client, "unknown", undefined, false, undefined, "invalid JSON");
					}
				}
			});

			client.on("close", () => {
				clients.delete(client);
			});

			client.on("error", () => {
				clients.delete(client);
			});
		});

		server.listen(socketPath, () => {
			// Set socket permissions.
			try {
				fs.chmodSync(socketPath!, 0o700);
			} catch {
				// ignore
			}
		});

		server.on("error", (err) => {
			console.error("[pup] socket server error:", err.message);
		});

		// Set up alias symlink.
		updateAlias(ctx);

		// Start name polling.
		namePollTimer = setInterval(() => {
			const newName = pi.getSessionName();
			if (newName !== currentName) {
				currentName = newName;
				if (newName) {
					broadcastEvent("session_name_changed", { name: newName });
				}
				if (savedCtx) updateAlias(savedCtx);
			}
		}, NAME_POLL_INTERVAL);

		// Start socket file monitor — recreate if deleted (e.g. directory wiped).
		socketCheckTimer = setInterval(() => {
			if (!socketPath || !server) return;
			try {
				fs.statSync(socketPath);
			} catch {
				// Socket file is gone. Recreate it.
				console.error("[pup] socket file missing, recreating...");
				if (savedCtx) recreateSocket(savedCtx);
			}
		}, SOCKET_CHECK_INTERVAL);
	}

	function recreateSocket(ctx: ExtensionContext) {
		if (!server) return;

		// Close old server (socket is gone, no cleanup needed).
		try { server.close(); } catch { /* ignore */ }
		for (const client of clients) {
			client.destroy();
		}
		clients.clear();
		server = null;

		// Recreate directory and socket.
		ensureSocketDir();
		socketPath = path.join(SOCKET_DIR, `${INSTANCE_ID}.sock`);

		server = net.createServer((client) => {
			clients.add(client);

			const ctx = savedCtx!;
			const helloData: Record<string, unknown> = {
				session_id: INSTANCE_ID,
				cwd: ctx.cwd,
			};
			const name = pi.getSessionName();
			if (name) helloData.session_name = name;
			const sessionFile = ctx.sessionManager.getSessionFile();
			if (sessionFile) helloData.session_file = sessionFile;
			helloData.thinking_level = pi.getThinkingLevel();
			sendToClient(client, "hello", helloData);

			const turns = getHistory(ctx);
			sendToClient(client, "history", {
				turns,
				streaming: isStreaming,
				...(isStreaming && accumulatedText ? { partial_text: accumulatedText } : {}),
			});

			let buffer = "";
			client.on("data", (data) => {
				buffer += data.toString();
				const lines = buffer.split("\n");
				buffer = lines.pop() ?? "";
				for (const line of lines) {
					if (!line.trim()) continue;
					try {
						const msg = JSON.parse(line);
						handleCommand(client, msg, savedCtx!);
					} catch {
						sendResponse(client, "unknown", undefined, false, undefined, "invalid JSON");
					}
				}
			});
			client.on("close", () => { clients.delete(client); });
			client.on("error", () => { clients.delete(client); });
		});

		server.listen(socketPath, () => {
			try { fs.chmodSync(socketPath!, 0o700); } catch { /* ignore */ }
		});

		server.on("error", (err) => {
			console.error("[pup] recreated socket server error:", err.message);
		});

		// Recreate alias.
		updateAlias(ctx);
	}

	// ── Slash command handling ──────────────────────────────────
	// When a message arrives via IPC that starts with "/", check if it
	// matches a pi slash command and execute it via the extension API
	// instead of sending it to the LLM as a user message.
	//
	// pi.sendUserMessage() goes directly to the agent — it does NOT
	// pass through the TUI's slash command parser. So "/new" would be
	// interpreted as a conversation message by the LLM.
	//
	// Commands fall into three categories:
	//   1. Supported — have equivalent ExtensionContext APIs, work fully
	//   2. Unsupported (need ExtensionCommandContext) — newSession(), fork(),
	//      navigateTree(), switchSession(), reload() are only available in
	//      registerCommand handlers, not in event/IPC handlers
	//   3. Unsupported (TUI-only) — require interactive UI (selectors, etc)
	//
	// For unsupported commands we broadcast a notification event so the
	// user sees a clear error in Telegram instead of the LLM getting a
	// slash command as conversation text.

	function notifyUnsupported(cmd: string, reason: string) {
		broadcastEvent("notification", {
			text: `⚠️ /${cmd} is not available via remote access — ${reason}`,
		});
	}

	function handleSlashCommand(
		client: net.Socket,
		id: string | undefined,
		message: string,
		ctx: ExtensionContext,
	): boolean {
		const trimmed = message.trim();
		if (!trimmed.startsWith("/")) return false;

		// Parse: "/name foo bar" → cmd="name", args="foo bar"
		// Also strip @botname suffix that Telegram appends in groups
		// (e.g. "/new@mybot" → cmd="new"). The daemon already strips
		// this but we handle it defensively here too.
		const spaceIdx = trimmed.indexOf(" ");
		const rawCmd = spaceIdx === -1 ? trimmed.slice(1) : trimmed.slice(1, spaceIdx);
		const atIdx = rawCmd.indexOf("@");
		const cmd = atIdx === -1 ? rawCmd : rawCmd.slice(0, atIdx);
		const args = spaceIdx === -1 ? "" : trimmed.slice(spaceIdx + 1).trim();

		switch (cmd) {
			// ── Supported commands (ExtensionContext APIs) ───────

			case "compact": {
				ctx.compact(args ? { customInstructions: args } : undefined);
				sendResponse(client, "send", id, true);
				return true;
			}

			case "name": {
				if (!args) {
					broadcastEvent("notification", { text: "⚠️ Usage: /name <name>" });
					sendResponse(client, "send", id, false, undefined, "Usage: /name <name>");
					return true;
				}
				pi.setSessionName(args);
				sendResponse(client, "send", id, true);
				return true;
			}

			case "quit":
			case "exit":
				ctx.shutdown();
				sendResponse(client, "send", id, true);
				return true;

			case "model": {
				if (!args) {
					notifyUnsupported(cmd, "interactive model selector requires pi TUI");
					sendResponse(client, "send", id, false, undefined, "/${cmd} without arguments requires pi TUI");
					return true;
				}
				pi.setModel(args as any).catch(() => {});
				sendResponse(client, "send", id, true);
				return true;
			}

			case "status": {
				const usage = ctx.getContextUsage();
				const model = ctx.model;
				const thinkingLevel = pi.getThinkingLevel();

				const lines: string[] = ["📊 Status"];

				if (model) {
					lines.push(`Model: ${model.provider}/${model.id}`);
				} else {
					lines.push("Model: unknown");
				}

				lines.push(`Thinking: ${thinkingLevel}`);

				if (usage) {
					const tokensStr = usage.tokens !== null ? usage.tokens.toLocaleString() : "unknown";
					const windowStr = usage.contextWindow.toLocaleString();
					const percentStr = usage.percent !== null ? `${usage.percent.toFixed(1)}%` : "unknown";
					lines.push(`Context: ${tokensStr} / ${windowStr} tokens (${percentStr})`);
				} else {
					lines.push("Context: unavailable");
				}

				broadcastEvent("notification", { text: lines.join("\n") });
				sendResponse(client, "send", id, true);
				return true;
			}

			// ── Unsupported: need ExtensionCommandContext ────────
			// These APIs (newSession, fork, navigateTree, switchSession,
			// reload) are only on ExtensionCommandContext, which is only
			// available inside registerCommand handlers. There is no
			// pi.executeCommand() API to trigger them programmatically.

			case "new":
			case "fork":
			case "tree":
			case "resume":
			case "reload":
				notifyUnsupported(cmd, "requires upstream pi API change (ExtensionCommandContext)");
				sendResponse(client, "send", id, false, undefined, `/${cmd} requires pi TUI (needs upstream API change)`);
				return true;

			// ── Unsupported: TUI-only (interactive UI) ──────────

			case "settings":
			case "scoped-models":
			case "export":
			case "share":
			case "copy":
			case "session":
			case "changelog":
			case "hotkeys":
			case "login":
			case "logout":
			case "debug":
				notifyUnsupported(cmd, "requires pi TUI");
				sendResponse(client, "send", id, false, undefined, `/${cmd} requires pi TUI`);
				return true;

			default:
				// Not a known slash command — fall through to sendUserMessage.
				return false;
		}
	}

	function handleCommand(
		client: net.Socket,
		msg: Record<string, unknown>,
		ctx: ExtensionContext,
	) {
		const type = msg.type as string;
		const id = msg.id as string | undefined;

		switch (type) {
			case "send": {
				const message = msg.message as string;
				const mode = (msg.mode as string) ?? "steer";

				if (!message) {
					sendResponse(client, "send", id, false, undefined, "message is required");
					return;
				}

				// Check for slash commands first.
				if (handleSlashCommand(client, id, message, ctx)) {
					return;
				}

				// Track for echo suppression.
				pendingSends.add(normalizeText(message));

				const deliverAs = mode === "follow_up" ? "followUp" : "steer";
				try {
					if (ctx.isIdle()) {
						pi.sendUserMessage(message);
					} else {
						pi.sendUserMessage(message, { deliverAs: deliverAs as any });
					}
					sendResponse(client, "send", id, true);
				} catch (err: any) {
					sendResponse(client, "send", id, false, undefined, err.message);
				}
				break;
			}
			case "abort": {
				try {
					ctx.abort();
					sendResponse(client, "abort", id, true);
				} catch (err: any) {
					sendResponse(client, "abort", id, false, undefined, err.message);
				}
				break;
			}
			case "get_info": {
				const info: Record<string, unknown> = {
					session_id: INSTANCE_ID,
					cwd: ctx.cwd,
				};
				const name = pi.getSessionName();
				if (name) info.session_name = name;
				info.thinking_level = pi.getThinkingLevel();
				sendResponse(client, "get_info", id, true, info);
				break;
			}
			case "get_history": {
				const maxTurns = (msg.turns as number) ?? DEFAULT_HISTORY_TURNS;
				const turns = getHistory(ctx, maxTurns);
				sendResponse(client, "get_history", id, true, {
					turns,
					streaming: isStreaming,
					...(isStreaming && accumulatedText ? { partial_text: accumulatedText } : {}),
				});
				break;
			}
			default:
				sendResponse(client, type ?? "unknown", id, false, undefined, `unknown command: ${type}`);
		}
	}

	function updateAlias(ctx: ExtensionContext) {
		// Remove old alias.
		if (aliasPath) {
			try {
				fs.unlinkSync(aliasPath);
			} catch {
				// ignore
			}
			aliasPath = null;
		}

		// Create new alias if session has a name.
		const name = pi.getSessionName();
		currentName = name;
		if (name && socketPath) {
			aliasPath = path.join(SOCKET_DIR, `${name}.alias`);
			try {
				fs.symlinkSync(socketPath, aliasPath);
			} catch {
				// Alias might already exist for another session.
				aliasPath = null;
			}
		}
	}

	// ── Event subscriptions ─────────────────────────────────────
	// No explicit teardown — when pi exits the process dies and the OS
	// cleans up the socket. The daemon detects the broken IPC connection
	// and deletes the topic.

	pi.on("session_start", async (_event, ctx) => {
		if (server) {
			// Session reset (/new or /compact) — reuse existing socket.
			// The daemon keeps the same connection and topic.
			savedCtx = ctx;
			sessionId = ctx.sessionManager.getSessionId() ?? null;
			isStreaming = false;
			currentMessageId = null;
			accumulatedText = "";
			accumulatedThinking = "";
			broadcastEvent("session_reset");
			updateAlias(ctx);
			return;
		}
		createSocketServer(ctx);
	});

	pi.on("session_shutdown", async () => {
		// Don't teardown — the socket stays alive for the pi process lifetime.
		// If this is /new or /compact, session_start will fire next and reuse
		// the socket. If pi is exiting, the process dies and the daemon detects
		// the broken connection.
		isStreaming = false;
		currentMessageId = null;
		accumulatedText = "";
		accumulatedThinking = "";
	});

	pi.on("agent_start", async () => {
		broadcastEvent("agent_start");
	});

	pi.on("agent_end", async () => {
		isStreaming = false;
		currentMessageId = null;
		accumulatedText = "";
		accumulatedThinking = "";
		broadcastEvent("agent_end");
	});

	pi.on("turn_start", async (event) => {
		broadcastEvent("turn_start", { turn_index: event.turnIndex });
	});

	pi.on("turn_end", async (event) => {
		broadcastEvent("turn_end", { turn_index: event.turnIndex });
	});

	pi.on("message_start", async (event) => {
		const msg = event.message;
		const role = msg.role;
		const messageId = `msg_${Date.now()}_${Math.random().toString(36).slice(2, 8)}`;

		if (role === "assistant") {
			isStreaming = true;
			currentMessageId = messageId;
			accumulatedText = "";
			accumulatedThinking = "";
		}

		broadcastEvent("message_start", { role, message_id: messageId });
	});

	pi.on("message_update", async (event) => {
		if (!currentMessageId) return;

		const msg = event.message;

		// Extract thinking content (extended thinking / chain-of-thought).
		const currentThinking = Array.isArray(msg.content)
			? msg.content
					.filter((c: any) => c.type === "thinking")
					.map((c: any) => c.thinking)
					.join("")
			: "";

		if (currentThinking.length > accumulatedThinking.length) {
			const thinkingDelta = currentThinking.slice(accumulatedThinking.length);
			accumulatedThinking = currentThinking;
			broadcastEvent("thinking_delta", {
				message_id: currentMessageId,
				text: thinkingDelta,
			});
		}

		// Extract the current full text from event.message.content.
		// This works regardless of provider streaming format.
		const currentText = Array.isArray(msg.content)
			? msg.content
					.filter((c: any) => c.type === "text")
					.map((c: any) => c.text)
					.join("")
			: typeof msg.content === "string"
				? msg.content
				: "";

		// Diff against accumulated text to get the delta.
		if (currentText.length > accumulatedText.length) {
			const textDelta = currentText.slice(accumulatedText.length);
			accumulatedText = currentText;
			broadcastEvent("message_delta", {
				message_id: currentMessageId,
				text: textDelta,
			});
		}
	});

	pi.on("message_end", async (event) => {
		const msg = event.message;
		const role = msg.role;
		const content = Array.isArray(msg.content)
			? msg.content
					.filter((c: any) => c.type === "text")
					.map((c: any) => c.text)
					.join("")
			: typeof msg.content === "string"
				? msg.content
				: "";

		if (role === "assistant") {
			isStreaming = false;
		}

		broadcastEvent("message_end", {
			message_id: currentMessageId ?? "",
			role,
			content,
		});

		if (role === "assistant") {
			currentMessageId = null;
			accumulatedText = "";
		}
	});

	pi.on("tool_execution_start", async (event) => {
		broadcastEvent("tool_start", {
			tool_call_id: event.toolCallId,
			tool_name: event.toolName,
			args: event.args ?? {},
		});
	});

	pi.on("tool_execution_update", async (event) => {
		const content = event.partialResult?.content
			? event.partialResult.content
					.filter((c: any) => c.type === "text")
					.map((c: any) => c.text)
					.join("")
			: "";
		broadcastEvent("tool_update", {
			tool_call_id: event.toolCallId,
			tool_name: event.toolName,
			content,
		});
	});

	pi.on("tool_execution_end", async (event) => {
		const content = event.result?.content
			? event.result.content
					.filter((c: any) => c.type === "text")
					.map((c: any) => c.text)
					.join("")
			: "";
		broadcastEvent("tool_end", {
			tool_call_id: event.toolCallId,
			tool_name: event.toolName,
			content,
			is_error: event.isError ?? false,
		});
	});

	pi.on("model_select", async (event) => {
		const model = event.model;
		const modelStr = `${model.provider}/${model.id}`;
		broadcastEvent("model_changed", { model: modelStr });
	});

	pi.on("input", async (event) => {
		const source = event.source ?? "interactive";

		if (source === "extension") {
			// Check if this is an echo of a message we sent via IPC.
			const normalized = normalizeText(event.text);
			if (pendingSends.delete(normalized)) {
				broadcastEvent("user_message", {
					content: event.text,
					source: "extension",
					echo: true,
				});
				return;
			}
		}

		broadcastEvent("user_message", {
			content: event.text,
			source,
			echo: false,
		});
	});
}
