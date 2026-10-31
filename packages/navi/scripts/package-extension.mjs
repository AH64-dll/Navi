import { copyFile, mkdir, readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const root = new URL("../", import.meta.url);
const output = new URL("dist/extension/", root);
await mkdir(output, { recursive: true });
const result = await build({
	absWorkingDir: fileURLToPath(root),
	entryPoints: {
		worker: "browser/worker/main.ts",
		content: "browser/content/main.ts",
		popup: "browser/popup/popup.ts",
	},
	outdir: fileURLToPath(output),
	bundle: true,
	platform: "browser",
	format: "iife",
	target: "chrome120",
	metafile: true,
	legalComments: "none",
	logLevel: "warning",
});
// Native messaging loads no network code. Keep packaging failure local and visible.
for (const [path, details] of Object.entries(result.metafile.outputs)) {
	if (details.imports.length !== 0) throw new Error(`External import in extension bundle: ${path}`);
	if (details.bytes > 256 * 1024) throw new Error(`Unexpected extension bundle growth: ${path}`);
}
const manifest = JSON.parse(await readFile(new URL("browser/manifest.json", root), "utf8"));
const permissions = [...manifest.permissions].sort();
if (JSON.stringify(permissions) !== JSON.stringify(["activeTab", "nativeMessaging", "scripting"])) {
	throw new Error("Unexpected extension permissions");
}
if (manifest.host_permissions || manifest.content_scripts || manifest.externally_connectable) {
	throw new Error("Ambient capture or external entrypoint is not supported");
}
await copyFile(new URL("browser/manifest.json", root), new URL("manifest.json", output));
await copyFile(new URL("browser/popup/popup.html", root), new URL("popup.html", output));
await copyFile(new URL("browser/popup/popup.css", root), new URL("popup.css", output));
console.log(`NAVI extension packaged at ${fileURLToPath(output)} (not installed or browser-verified).`);
