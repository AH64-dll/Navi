import { realpathSync } from "node:fs";
import { isAbsolute } from "node:path";
import { fileURLToPath } from "node:url";
import { writeNativeSetup } from "./native-setup.ts";
import { formatOutput } from "./output.ts";
import { serve } from "./serve.ts";

export const HELP = `NAVI foundation (diagnostic scaffold; no model runtime)

  node packages/navi/src/cli/main.ts serve --socket-dir <absolute-private-dir>
  node packages/navi/src/cli/main.ts native-setup --socket-dir <dir> --extension-id <id> --out <dir>

Start serve first, then attach explicitly from the extension popup.
Native setup generates developer files only. Register them manually in your browser.
POSIX local IPC and Chromium are the initial targets. Windows is not supported yet.`;

function flags(args: string[], allowed: string[]): Record<string, string> {
	const parsed: Record<string, string> = {};
	for (let i = 0; i < args.length; i += 2) {
		const name = args[i]!;
		const value = args[i + 1];
		if (!allowed.includes(name) || value === undefined || value.startsWith("--") || Object.hasOwn(parsed, name)) {
			throw new Error("Unknown, missing, or duplicate command option");
		}
		parsed[name] = value;
	}
	return parsed;
}

export async function main(args: string[]): Promise<void> {
	const [command, ...rest] = args;
	if (!command || command === "--help" || command === "help") {
		console.log(HELP);
		return;
	}
	const options = flags(rest, command === "serve" ? ["--socket-dir"] : ["--socket-dir", "--extension-id", "--out"]);
	const socketDir = options["--socket-dir"];
	if (!socketDir || !isAbsolute(socketDir)) throw new Error("--socket-dir must be an absolute private directory");
	if (command === "serve") {
		await serve({ socketDir });
		return;
	}
	if (command === "native-setup" && options["--extension-id"] && options["--out"]) {
		console.log(
			formatOutput(
				await writeNativeSetup({ socketDir, extensionId: options["--extension-id"], outDir: options["--out"] }),
			),
		);
		return;
	}
	throw new Error("Use --help for the supported commands");
}

const entry = process.argv[1];
if (entry && realpathSync(entry) === realpathSync(fileURLToPath(import.meta.url))) {
	try {
		await main(process.argv.slice(2));
	} catch (error) {
		console.error(formatOutput({ error: error instanceof Error ? error.message : "NAVI failed" }));
		process.exitCode = 1;
	}
}
