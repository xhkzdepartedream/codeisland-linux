/**
 * codeisland — the permission island as a Pi extension.
 *
 * Load it explicitly for a one-off run:
 *
 *     pi -e <repo>/integrations/pi/codeisland.ts
 *
 * or let Pi discover it as a package (project scope, nothing written to global config):
 *
 *     mkdir -p .pi/extensions && cp <repo>/integrations/pi/codeisland.ts .pi/extensions/
 *     # or: pi install <repo>/integrations/pi
 *
 * WHY THIS FILE IS NOT A HOOK COMMAND
 *
 * Pi has no hook configuration file. Claude Code and codex run an external command per
 * event and read its stdout, which is what codeisland-bridge speaks. Pi instead loads a
 * module into the agent process and lets that module veto a tool call. The daemon protocol
 * does not change -- one newline-delimited JSON frame out, one reply line back -- so this
 * adapter is the second integration shape over the same wire, which is the actual phase-4
 * claim: the protocol is not claude-shaped-with-extra-steps.
 *
 * THE ONE THING THAT IS INVERTED HERE, AND WHAT WE DO ABOUT IT
 *
 * Pi's approval model is veto-on-return: a `tool_call` handler that returns nothing lets
 * the call proceed. Claude's and codex's model is the opposite -- the CLI asks, and an
 * external hook that says nothing leaves the decision with the CLI. CodeIsland's contract
 * is that the island must never be the thing that silently decides. So a missing or silent
 * island here must NOT mean "allow": it means "ask the way Pi would have asked anyway",
 * which is `ctx.ui.confirm`. Only when there is no UI at all (headless -p, or
 * `ctx.hasUI === false`) does the call proceed untouched, because then "leave the decision
 * to the CLI" is exactly what the CLI does with no input.
 *
 * THE OTHER RULE THIS FILE OBEYS, BECAUSE IT COST SOMEONE 89 SECONDS
 *
 * Pi does not send the model request until its turn-start hooks resolve. An extension that
 * awaits a slow call from `context`, `input`, or `session_start` therefore freezes the whole
 * turn -- that is how a remote embedding call in an unrelated extension looked like a dead
 * proxy. So: every status frame in here is fire-and-forget and never awaited. The only
 * `await` is the approval itself, which is the point of the extension, and it is bounded by
 * two separate timers rather than one: a connect deadline (a socket that exists but nobody
 * accepts on must not eat the whole answer budget) and an answer deadline.
 */

import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

/** Mirrors codeisland-core's EMULATOR_NAMES. Keep in sync: the jump target is computed from
 *  this list on the Rust side, and a divergent copy means the island activates the wrong
 *  window. */
const EMULATOR_NAMES = [
	"konsole",
	"kitty",
	"alacritty",
	"wezterm",
	"wezterm-gui",
	"foot",
	"st",
	"urxvt",
	"rxvt-unicode",
	"gnome-terminal-",
	"terminator",
	"tilix",
	"qterminal",
	"xfce4-terminal",
	"deepin-terminal",
];

/** Environment keys codeisland-core records as terminal hints. Same list, same reason. */
const HINT_KEYS = [
	"TERM_PROGRAM",
	"KONSOLE_DBUS_SESSION",
	"KONSOLE_DBUS_WINDOW",
	"TMUX",
	"TMUX_PANE",
	"STY",
	"SCREEN_SESSION",
];

const STATUS_KEY = "codeisland";

function socketPath(): string {
	return process.env.CODEISLAND_SOCKET_PATH || `/tmp/codeisland-${os.userInfo().uid}.sock`;
}

function envMs(name: string, fallback: number): number {
	const raw = Number(process.env[name] ?? "");
	return Number.isFinite(raw) && raw > 0 ? raw : fallback;
}

/** How long the connect may take. Independent of the answer budget on purpose: a leftover
 *  socket file with no listener behind it would otherwise hold the tool call for the full
 *  approval window. */
function connectTimeoutMs(): number {
	return envMs("CODEISLAND_PI_CONNECT_MS", 3_000);
}

/** How long to wait for a human before falling back to Pi's own prompt. Far shorter than
 *  the bridge's hour: blocking a tool call for an hour from inside the agent process is not
 *  the same as a separate hook process blocking, and a human who has not answered in two
 *  minutes is not going to answer this call. */
function approvalTimeoutMs(): number {
	return envMs("CODEISLAND_PI_APPROVAL_MS", 120_000);
}

/** Walk /proc the way codeisland-core does, to find the terminal emulator hosting Pi.
 *  ppid is the field after state -- fields[1] once the comm has been cut off at ')'. The
 *  same off-by-one cost a debugging round in Rust, so it is commented here as well. */
function terminalContext(): Record<string, unknown> {
	const hints: Record<string, string> = {};
	for (const key of HINT_KEYS) {
		const value = process.env[key];
		if (value) hints[key] = value;
	}

	let pid: number | undefined = process.pid;
	const seen = new Set<number>();
	let emulatorPid: number | undefined;
	let emulator: string | undefined;

	for (let depth = 0; depth < 24 && pid && !seen.has(pid); depth++) {
		seen.add(pid);
		let comm = "";
		let stat = "";
		try {
			comm = fs.readFileSync(`/proc/${pid}/comm`, "utf8").trim();
			stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
		} catch {
			break;
		}
		const close = stat.lastIndexOf(")");
		const fields = close >= 0 ? stat.slice(close + 1).trim().split(/\s+/) : [];
		const parent = Number(fields[1]);
		if (!emulator && EMULATOR_NAMES.some((name) => comm.startsWith(name))) {
			emulator = comm;
			emulatorPid = pid;
			break;
		}
		pid = Number.isFinite(parent) && parent > 0 ? parent : undefined;
	}

	const context: Record<string, unknown> = { hints };
	if (emulatorPid) context.emulator_pid = emulatorPid;
	if (emulator) context.emulator = emulator;
	const mux = hints.TMUX ? "tmux" : hints.STY || hints.SCREEN_SESSION ? "screen" : undefined;
	if (mux) context.multiplexer = mux;
	return context;
}

let sessionIdCache: string | undefined;
function sessionId(ctx: ExtensionContext): string {
	if (sessionIdCache) return sessionIdCache;
	try {
		const file = ctx.sessionManager.getSessionFile();
		sessionIdCache = file ? path.basename(file).replace(/\.jsonl$/, "") : `pi-${process.pid}`;
	} catch {
		sessionIdCache = `pi-${process.pid}`;
	}
	return sessionIdCache;
}

function buildFrame(event: string, ctx: ExtensionContext, extra: Record<string, unknown>): Record<string, unknown> {
	return {
		source: "pi",
		event,
		// The bridge normally fills this; this adapter speaks the daemon's own vocabulary, so
		// it says so rather than relying on the normaliser's pass-through.
		normalized_event: event,
		session_id: sessionId(ctx),
		cwd: process.cwd(),
		ts_ms: Date.now(),
		terminal: terminalContext(),
		...extra,
	};
}

/** Fire-and-forget status frame. Never awaited, never throws: a status event must not delay
 *  a turn -- see the header note about turn-start hooks. */
function report(event: string, ctx: ExtensionContext, extra: Record<string, unknown> = {}): void {
	let frame: string;
	try {
		frame = `${JSON.stringify(buildFrame(event, ctx, extra))}\n`;
	} catch {
		return;
	}
	try {
		const socket = net.connect(socketPath());
		socket.setTimeout(connectTimeoutMs());
		socket.on("error", () => socket.destroy());
		socket.on("timeout", () => socket.destroy());
		socket.end(frame);
	} catch {
		/* invisible by design */
	}
}

type Answer = { behavior: string } | null;

/** One frame out, one reply in. Resolves null for every failure mode, and the caller turns
 *  null into "do what Pi would have done". */
function ask(event: string, ctx: ExtensionContext, extra: Record<string, unknown>): Promise<Answer> {
	return new Promise((resolve) => {
		let settled = false;
		let buffer = "";
		let connected = false;
		const socket = net.connect(socketPath());

		const finish = (answer: Answer) => {
			if (settled) return;
			settled = true;
			socket.destroy();
			resolve(answer);
		};

		// Two budgets, not one: connect, then answer.
		const connectTimer = setTimeout(() => finish(null), connectTimeoutMs());
		let answerTimer: NodeJS.Timeout | undefined;

		socket.on("connect", () => {
			connected = true;
			clearTimeout(connectTimer);
			answerTimer = setTimeout(() => finish(null), approvalTimeoutMs());
			socket.write(`${JSON.stringify(buildFrame(event, ctx, extra))}\n`);
		});
		socket.on("error", () => {
			clearTimeout(connectTimer);
			finish(null);
		});
		socket.on("end", () => {
			clearTimeout(connectTimer);
			if (answerTimer) clearTimeout(answerTimer);
			finish(parseAnswer());
		});
		socket.on("close", () => {
			clearTimeout(connectTimer);
			if (answerTimer) clearTimeout(answerTimer);
			finish(parseAnswer());
		});

		const parseAnswer = (): Answer => {
			const trimmed = buffer.trim();
			if (!trimmed) return null;
			try {
				const parsed = JSON.parse(trimmed) as {
					hookSpecificOutput?: { decision?: { behavior?: string } };
				};
				const behavior = parsed?.hookSpecificOutput?.decision?.behavior;
				return behavior === "allow" || behavior === "deny" ? { behavior } : null;
			} catch {
				return null;
			}
		};

		socket.on("data", (chunk) => {
			if (process.env.CODEISLAND_PI_DEBUG) {
				// One measurement settles what a log of guesses would not: whether the reply
				// frame arrived, and with what bytes.
				try {
					fs.appendFileSync(
						process.env.CODEISLAND_PI_DEBUG,
						`${JSON.stringify({ seen: chunk.toString("utf8"), connected })}\n`,
					);
				} catch {
					/* diagnostics never break the loop */
				}
			}
			buffer += chunk.toString("utf8");
			const answer = parseAnswer();
			// The reply direction is EOF-terminated, not newline-terminated: the request frame
			// we send is newline-delimited, but the daemon writes its decision with no trailing
			// newline because a hook's stdout is read until the process exits (the Rust bridge
			// uses read_to_string). Requiring "\n" here silently turned every answer into "no
			// answer", which headless then read as permission to proceed. Parse when it parses,
			// and once more at end-of-stream.
			if (answer) {
				if (answerTimer) clearTimeout(answerTimer);
				finish(answer);
			}
		});
	});
}

/** The documented heuristic from Pi's own permission-extension example: trust the
 *  annotations, and treat unknown tools as possibly destructive. */
function needsApproval(toolName: string, pi: ExtensionAPI): boolean {
	const annotations = (
		pi.getAllTools() as { name: string; annotations?: Record<string, boolean> }[]
	).find((tool) => tool.name === toolName)?.annotations;
	if (!annotations) return true;
	return (
		annotations.destructiveHint === true ||
		(!annotations.readOnlyHint && (annotations.destructiveHint ?? true)) ||
		(!annotations.readOnlyHint && (annotations.openWorldHint ?? true))
	);
}

function summarize(toolName: string, input: unknown): string {
	const record = (input ?? {}) as Record<string, unknown>;
	for (const key of ["command", "path", "file_path", "pattern", "query", "url"]) {
		const value = record[key];
		if (typeof value === "string" && value) return `${toolName}: ${value.slice(0, 160)}`;
	}
	return toolName;
}

export default function (pi: ExtensionAPI) {
	pi.on("session_start", async (_event, ctx) => report("SessionStart", ctx));
	pi.on("input", async (event, ctx) => {
		const text =
			(event as unknown as { text?: string; prompt?: string }).text ??
			(event as unknown as { prompt?: string }).prompt;
		report("UserPromptSubmit", ctx, text ? { message: String(text).slice(0, 512) } : {});
	});
	pi.on("tool_result", async (event, ctx) => {
		report("PostToolUse", ctx, { tool_name: (event as { toolName?: string }).toolName });
	});
	pi.on("agent_end", async (_event, ctx) => report("Stop", ctx));
	pi.on("session_shutdown", async (_event, ctx) => report("SessionEnd", ctx));

	pi.on("tool_call", async (event, ctx) => {
		const toolName = (event as { toolName: string }).toolName;
		const input = (event as { input?: unknown }).input;
		if (!needsApproval(toolName, pi)) return;

		const summary = summarize(toolName, input);
		// Visible while blocked, cleared right after: the veto can wait minutes, and a silent
		// pause looks exactly like a hang from the outside.
		try {
			ctx.ui.setStatus(STATUS_KEY, `⏳ 岛上等审批：${summary.slice(0, 60)}`);
		} catch {
			/* no UI, no marker */
		}

		const answer = await ask("PermissionRequest", ctx, {
			tool_name: toolName,
			tool_input: input ?? {},
			summary,
		});

		try {
			ctx.ui.setStatus(STATUS_KEY, undefined);
		} catch {
			/* ignore */
		}

		if (answer?.behavior === "allow") return;
		if (answer?.behavior === "deny") {
			return { block: true, reason: `${toolName} 被岛上驳回` };
		}

		// No island, no answer, no viewer, or an unparsable reply. The contract says the
		// decision returns to the CLI -- here that is Pi's own confirmation, not a silent
		// allow and not a denial we invented.
		if (ctx.hasUI) {
			const confirmed = await ctx.ui.confirm("Allow tool call?", summary);
			if (!confirmed) return { block: true, reason: `${toolName} was not approved` };
		}
		return;
	});

	pi.registerCommand("codeisland", {
		description: "Show how this session is wired to the permission island",
		handler: async (_args, ctx) => {
			const probe = await reachable();
			const terminal = terminalContext();
			const lines = [
				`socket ${socketPath()} — ${probe}`,
				`session ${sessionId(ctx)}`,
				`terminal ${String(terminal.emulator ?? "?")} pid ${String(terminal.emulator_pid ?? "?")}`,
				`timeouts connect ${connectTimeoutMs()}ms / answer ${approvalTimeoutMs()}ms`,
				`uninstall pi remove <path>  或把 .pi/extensions/codeisland.* 移出扫描目录`,
			];
			if (ctx.hasUI) ctx.ui.notify(lines.join("\n"), "info");
			return lines.join("\n");
		},
	});
}

/** Connect and hang up, without sending a frame: a reachability answer for /codeisland. */
function reachable(): Promise<string> {
	return new Promise((resolve) => {
		const target = socketPath();
		if (!fs.existsSync(target)) {
			resolve("没找到 socket（常驻程序没跑？）");
			return;
		}
		const socket = net.connect(target);
		const timer = setTimeout(() => {
			socket.destroy();
			resolve("socket 在，但连不上（常驻程序卡住？）");
		}, connectTimeoutMs());
		socket.on("connect", () => {
			clearTimeout(timer);
			socket.destroy();
			resolve("已连上");
		});
		socket.on("error", (err: NodeJS.ArrayBufferError) => {
			clearTimeout(timer);
			resolve(`连不上：${(err as Error).message}`);
		});
	});
}
