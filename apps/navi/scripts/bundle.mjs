import { build } from "esbuild";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
for (const [entry, output, options] of [
  ["ui/scene.js", "ui/scene.bundle.js", {format: "iife", minify: true}],
  ["browser/content-entry.ts", "browser/content.js", {target: "firefox128"}],
  ["browser/background-entry.js", "browser/background.js", {target: "firefox128"}],
]) {
  await build({entryPoints: [root + entry], outfile: root + output, bundle: true, ...options});
}
