/**
 * codeisland — the permission island as a Pi extension.
 *
 * Usage (explicit load; this project does not register itself into global settings):
 *
 *     pi -e <repo>/integrations/pi/codeisland.ts
 *
 * Why this file is not a hook command: Pi has no hook configuration file. Claude Code
 * and codex run an external command per event and read its stdout, which is what
 * codeisland-bridge speaks. Pi instead loads a module into its own process and lets that
 * module veto a tool call. The daemon protocol does not change -- one newline-delimited
 * JSON frame, one reply line -- so this adapter proves the protocol is CLI-agnostic
 * rather than claude-shaped-with-extra-steps.
 *
 * THE ONE THING THAT IS INVERTED HERE, AND WHAT WE DO ABOUT IT:
 *
 * Pi's approval model is veto-on-return: a `tool_call` handler that returns nothing lets
 * the call proceed. Claude's and codex's model is the opposite -- the CLI asks, and an
 * external hook that says nothing leaves the decision with the CLI. CodeIsland's contract
 * is that the island must never be the thing that silently decides: if the viewer
 * disconnects, the agent dies, or nobody answers, the request goes back to the CLI.
 *
 * So a missing or silent island here must NOT mean "allow". It means "ask the way Pi
 * would have asked anyway", which is `ctx.ui.confirm`. Only when there is no UI at all
 * (headless `-p`, or `ctx.hasUI === false`) does the call proceed unmodified, because
 * then "leave the decision to the CLI" is exactly what the CLI does with no input.
 *
 * Everything else is deliberately fail-open toward Pi's own behaviour: this module must
 * never be the reason a session wedges. Socket errors, timeouts, and unparsable replies
 * all fall through to the path Pi would have taken without us.
 */

import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

/** Mirrors codeisland-core's EMULATOR_NAMES. Keep in sync: the jump target is computed
 *  from this list on the Rust side, and a divergent copy means the island activates the
 *  wrong window. */
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

/** Environment keys codeisland-core records as terminal hints, plus TERM_PROGRAM. */
const HINT_KEYS = ["TERM_PROGRAM", "KONSOLE_DBUS_SESSION", "KONSOLE_DBUS_WINDOW", "TMUX", "TMUX_PANE", "STY", "SCREEN_SESSION"];

function socketPath(): string {
	return process.env.CODEISLAND_SOCKET_PATH || `/tmp/codeisland-${os.userInfo().uid}.sock`;
}

/** How long to wait for a human before falling back to Pi's own prompt.
 *  Deliberately far shorter than the bridge's hour: blocking a tool call for an hour from
 *  inside the agent process is not the same as a separate hook process blocking, and a
 *  human who has not answered in five minutes is not going to answer this call. */
function approvalTimeoutMs(): number {
	const raw = Number(process.env.CODEISLAND_PI_APPROVAL_MS ?? "");
	return Number.isFinite(raw) && raw > 0 ? raw : 300_000;
}

/** Walk /proc the way codeisland-core does, to find the terminal emulator hosting Pi.
 *  ppid is the field after state, i.e. index 1 after the ')' split -- the same off-by-one
 *  that cost a debugging round in Rust. */
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
		const parent = Number(fields[1]); // fields[0] is state, fields[1] is ppid
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

/** Fire-and-forget status frame. Never awaited: a status event must not delay a turn, and
 *  a failure here is invisible by design -- the island showing stale state is not a
 *  reason to change what the agent does. */
function report(event: string, ctx: ExtensionContext, extra: Record<string, unknown> = {}): void {
	try {
		const frame = buildFrame(event, ctx, extra);
		const socket = net.connect(socketPath());
		socket.on("error", () => socket.destroy());
		socket.end(`${JSON.stringify(frame)}\n`);
	} catch {
		/* never throw from a status path */
	}
}

function buildFrame(event: string, ctx: ExtensionContext, extra: Record<string, unknown>): Record<string, unknown> {
	return {
		source: "pi",
		event,
		// The bridge normally fills this; this adapter speaks the daemon's own vocabulary,
		// so it says so explicitly rather than relying on the normaliser's pass-through.
		normalized_event: event,
		session_id: sessionId(ctx),
		cwd: process.cwd(),
		ts_ms: Date.now(),
		terminal: terminalContext(),
		...extra,
	};
}

type Answer = { behavior: string } | null;

/** One frame out, one line back. Resolves null for every failure mode, and the caller
 *  turns null into "do what Pi would have done". */
function ask(event: string, ctx: ExtensionContext, extra: Record<string, unknown>): Promise<Answer> {
	return new Promise((resolve) => {
		let settled = false;
		let buffer = "";
		const socket = net.connect(socketPath());
		const finish = (answer: Answer) => {
			if (settled) return;
			settled = true;
			socket.destroy();
			resolve(answer);
		};

		const timer = setTimeout(() => finish(null), approvalTimeoutMs());
		socket.on("timeout", () => finish(null));
		socket.on("error", () => {
			clearTimeout(timer);
			finish(null);
		});
		// The reply direction is EOF-terminated, not newline-terminated: the request frame
		// we send is newline-delimited, but the daemon writes its decision with no trailing
		// newline because a hook's stdout is read until the process exits (the Rust bridge
		// uses read_to_string). Requiring "\n" here silently turned every answer into
		// "no answer", which headless then read as permission to proceed. Parse what has
		// arrived whenever it parses, and once more at end-of-stream.
		const parseAnswer = (): Answer => {
			const trimmed = buffer.trim();
			if (!trimmed) return null;
			try {
				const parsed = JSON.parse(trimmed) as { hookSpecificOutput?: { decision?: { behavior?: string } } };
				const behavior = parsed?.hookSpecificOutput?.decision?.behavior;
				return behavior === "allow" || behavior === "deny" ? { behavior } : null;
			} catch {
				return null;
			}
		};

		socket.on("end", () => {
			clearTimeout(timer);
			finish(parseAnswer());
		});
		socket.on("data", (chunk) => {
			if (process.env.CODEISLAND_PI_DEBUG) {
				fs.appendFileSync(process.env.CODEISLAND_PI_DEBUG, `${JSON.stringify({ seen: chunk.toString("utf8") })}\n`);
			}
			buffer += chunk.toString("utf8");
			const answer = parseAnswer();
			if (answer) {
				clearTimeout(timer);
				finish(answer);
			}
		});
		socket.write(`${JSON.stringify(buildFrame(event, ctx, extra))}\n`);
	});
}

/** The documented heuristic from Pi's own permission-extension example: trust the
 *  annotations, and treat unknown tools as possibly destructive. */
function needsApproval(toolName: string, pi: ExtensionAPI): boolean {
	const annotations = (pi.getAllTools() as { name: string; annotations?: Record<string, boolean> }[])
		.find((tool) => tool.name === toolName)?.annotations;
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
		if (typeof record[key] === "string" && record[key]) return `${toolName}: ${String(record[key]).slice(0, 160)}`;
	}
	return toolName;
}

export default function (pi: ExtensionAPI) {
	pi.on("session_start", async (_event, ctx) => report("SessionStart", ctx));
	pi.on("input", async (event, ctx) => {
		const text = (event as unknown as { text?: string; prompt?: string }).text
			?? (event as unknown as { prompt?: string }).prompt;
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
		const answer = await ask("PermissionRequest", ctx, { tool_name: toolName, tool_input: input ?? {}, summary });

		if (answer?.behavior === "allow") return; // the island said yes
		if (answer?.behavior === "deny") {
			return { block: true, reason: `${toolName} 被岛上驳回` };
		}

		// No island, no answer, no viewer, or a malformed reply. The contract says the
		// decision returns to the CLI -- here that is Pi's own confirmation, not a silent
		// allow and not a denial we invented.
		if (ctx.hasUI) {
			const confirmed = await ctx.ui.confirm("Allow tool call?", summary);
			if (!confirmed) return { block: true, reason: `${toolName} was not approved` };
		}
		return;
	});

}
