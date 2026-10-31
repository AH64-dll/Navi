import { mkdir, writeFile } from "node:fs/promises";
import { isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { assertPrivateSocketDir } from "../transport/socket.ts";

export type NativeSetupOptions = { outDir: string; socketDir: string; extensionId: string };

function shellQuote(value: string): string {
	return `'${value.replaceAll("'", "'\\''")}'`;
}

/** Generate developer artifacts only. No browser/profile/system registration is performed. */
export async function writeNativeSetup(options: NativeSetupOptions): Promise<{ manifest: string; launcher: string }> {
	if (process.platform === "win32") throw new Error("Native setup currently supports POSIX only");
	if (!/^[a-p]{32}$/.test(options.extensionId)) throw new Error("Extension ID must be 32 lowercase letters a-p");
	if (!isAbsolute(options.socketDir)) throw new Error("Socket directory must be absolute");
	const output = resolve(options.outDir);
	await mkdir(output, { recursive: true, mode: 0o700 });
	assertPrivateSocketDir(output);
	const launcher = join(output, "navi-connector");
	const manifest = join(output, "dev.navi.bridge.json");
	const connector = fileURLToPath(new URL("../transport/connector.ts", import.meta.url));
	const command = [
		process.execPath,
		connector,
		"--socket-dir",
		options.socketDir,
		"--extension-id",
		options.extensionId,
	]
		.map(shellQuote)
		.join(" ");
	await writeFile(launcher, `#!/bin/sh\nexec ${command} "$@"\n`, { flag: "wx", mode: 0o700 });
	await writeFile(
		manifest,
		`${JSON.stringify(
			{
				name: "dev.navi.bridge",
				description: "NAVI relay to a user-started local CLI",
				path: launcher,
				type: "stdio",
				allowed_origins: [`chrome-extension://${options.extensionId}/`],
			},
			null,
			2,
		)}\n`,
		{ flag: "wx", mode: 0o600 },
	);
	return { manifest, launcher };
}
