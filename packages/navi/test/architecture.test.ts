/**
 * Boundary checks for the NAVI foundation. These are structural assertions about the source tree and
 * the packaged extension, not behavior tests: they fail when a module crosses realms, when the
 * extension gains a permission or entrypoint it must not have, or when the packaged bundle gains an
 * external import.
 */
import assert from "node:assert/strict";
import { readdir, readFile, stat } from "node:fs/promises";
import { join, relative } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));

async function sourceFiles(directory: string): Promise<string[]> {
	const entries = await readdir(directory, { withFileTypes: true });
	const files: string[] = [];
	for (const entry of entries) {
		const path = join(directory, entry.name);
		if (entry.isDirectory()) files.push(...(await sourceFiles(path)));
		else if (entry.name.endsWith(".ts")) files.push(path);
	}
	return files;
}

/** Comments mention `document` and `chrome` as prose; only executable text is checked. */
function code(source: string): string {
	return source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/(^|[^:])\/\/[^\n]*/g, "$1");
}

function importSpecifiers(source: string): string[] {
	const specifiers: string[] = [];
	const patterns = [
		/\bfrom\s+"([^"]+)"/g,
		/\bfrom\s+'([^']+)'/g,
		/\bimport\s*\(\s*["']([^"']+)["']\s*\)/g,
		/\bimport\s+["']([^"']+)["']/g,
	];
	for (const pattern of patterns) {
		for (const match of source.matchAll(pattern)) specifiers.push(match[1]!);
	}
	return specifiers;
}

const NODE_BUILTIN = /^node:/;

test("the browser realm never imports Node, the CLI or the transport", async () => {
	for (const file of await sourceFiles(join(root, "browser"))) {
		if (file.includes("/test/")) continue;
		const specifiers = importSpecifiers(await readFile(file, "utf8"));
		for (const specifier of specifiers) {
			assert.ok(!NODE_BUILTIN.test(specifier), `${relative(root, file)} imports ${specifier}`);
			assert.ok(!specifier.includes("/src/transport/"), `${relative(root, file)} imports ${specifier}`);
			assert.ok(!specifier.includes("/src/cli/"), `${relative(root, file)} imports ${specifier}`);
			assert.ok(
				!/^[^./]/.test(specifier) || specifier.startsWith("../"),
				`${relative(root, file)} imports package ${specifier}`,
			);
		}
	}
});

test("shared modules stay free of Node, Chrome and DOM", async () => {
	for (const realm of ["src/protocol", "src/core"]) {
		for (const file of await sourceFiles(join(root, realm))) {
			const source = code(await readFile(file, "utf8"));
			for (const specifier of importSpecifiers(source)) {
				assert.ok(!NODE_BUILTIN.test(specifier), `${relative(root, file)} imports ${specifier}`);
			}
			assert.ok(!/\bchrome\s*\./.test(source), `${relative(root, file)} uses chrome.*`);
			assert.ok(!/\bdocument\s*\.|\bglobalThis\.document/.test(source), `${relative(root, file)} uses the DOM`);
		}
	}
});

test("no dynamic imports and no debugger or CDP automation in the foundation", async () => {
	const files = [...(await sourceFiles(join(root, "src"))), ...(await sourceFiles(join(root, "browser")))];
	for (const file of files) {
		const source = code(await readFile(file, "utf8"));
		assert.ok(!/\bawait\s+import\s*\(/.test(source), `${relative(root, file)} uses a dynamic import`);
		assert.ok(!/eval\s*\(\s*[^)]/.test(source), `${relative(root, file)} uses eval()`);
	}
	for (const directory of [join(root, "src"), join(root, "browser")]) {
		for (const file of await sourceFiles(directory)) {
			const source = code(await readFile(file, "utf8"));
			assert.ok(!/"debugger"/.test(source), `${relative(root, file)} requests the debugger permission`);
			assert.ok(!/attachToTarget|sendCommand\s*\(/.test(source), `${relative(root, file)} uses a CDP-style call`);
		}
	}
});

test("the manifest keeps the minimal permission surface", async () => {
	const manifest = JSON.parse(await readFile(join(root, "browser/manifest.json"), "utf8")) as Record<string, unknown>;
	assert.equal(manifest.manifest_version, 3);
	assert.deepEqual([...(manifest.permissions as string[])].sort(), ["activeTab", "nativeMessaging", "scripting"]);
	for (const forbidden of [
		"host_permissions",
		"optional_host_permissions",
		"content_scripts",
		"externally_connectable",
		"web_accessible_resources",
		"side_panel",
		"devtools_page",
	]) {
		assert.equal(manifest[forbidden], undefined, `manifest must not declare ${forbidden}`);
	}
	assert.deepEqual(Object.keys(manifest.background as Record<string, unknown>).sort(), ["service_worker", "type"]);
});

test("the packaged extension exists with no external imports", async (context) => {
	const output = join(root, "dist/extension");
	const manifestPath = join(output, "manifest.json");
	try {
		await stat(manifestPath);
	} catch {
		context.skip("run node packages/navi/scripts/package-extension.mjs first");
		return;
	}
	const manifest = JSON.parse(await readFile(manifestPath, "utf8")) as Record<string, unknown>;
	assert.deepEqual([...(manifest.permissions as string[])].sort(), ["activeTab", "nativeMessaging", "scripting"]);
	for (const name of ["worker.js", "content.js", "popup.js", "popup.html"]) {
		assert.ok((await stat(join(output, name))).isFile(), `${name} is missing from the bundle`);
	}
	for (const name of ["worker.js", "content.js", "popup.js"]) {
		const bundle = await readFile(join(output, name), "utf8");
		assert.ok(!/^import\s|require\s*\(/m.test(bundle), `${name} has an external import`);
		assert.ok(!/\beval\s*\(/.test(bundle), `${name} uses eval()`);
	}
});
