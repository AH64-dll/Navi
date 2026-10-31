import type { BrowserPort } from "../core/ports.ts";

export const DIAGNOSTIC_COMMANDS = "status | observe | focus <ref> | scroll <dy> | cancel | detach | quit";

/** Honest scaffold adapter. A future model harness consumes BrowserPort, never Chrome or transport. */
export async function runDiagnostic(port: BrowserPort, line: string): Promise<unknown> {
	if (line.length > 1024) throw new Error("Command exceeds the diagnostic input limit");
	const [command, ...args] = line.trim().split(/\s+/);
	if (command === "status" && args.length === 0) return port.status();
	if (command === "observe" && args.length === 0) {
		return { source: "page", trust: "untrusted", observation: await port.observe() };
	}
	if (command === "focus" && args.length === 1) return { outcome: await port.focus(args[0]!) };
	if (command === "scroll" && args.length === 1) {
		const deltaY = Number(args[0]);
		if (!Number.isFinite(deltaY)) throw new Error("Scroll delta must be finite");
		return await port.scroll({ deltaY });
	}
	if (command === "cancel" && args.length === 0) return await port.cancel();
	if (command === "detach" && args.length === 0) {
		await port.disconnect("detached");
		return { detached: true };
	}
	throw new Error(`Supported diagnostic commands: ${DIAGNOSTIC_COMMANDS}`);
}
