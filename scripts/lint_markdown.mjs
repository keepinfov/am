import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const modules = resolve(
    process.env.AM_NODE_MODULES ?? resolve(root, "node_modules"),
);
const require = createRequire(resolve(modules, "markdownlint/package.json"));
const { lint } = await import(
    pathToFileURL(require.resolve("markdownlint/promise"))
);
// Use literal filenames, not globs or executable configuration.
const config = JSON.parse(
    await readFile(resolve(root, ".markdownlint.json"), "utf8"),
);
const files = process.argv.slice(2);
if (!files.length) {
    throw new Error("Specify at least one Markdown file");
}
// The scoped KaTeX override moves past the extension's ^0.16 range to patch
// GHSA-238p-pmpm-9mq7. Markdownlint only uses math syntax, not rendering;
// the extension's renderToString API is also tested for compatibility.
const isFragment = (file) =>
    dirname(resolve(file)) === resolve(root, "changelog.d") &&
    basename(file) !== "README.md";
const results = {};
for (const [selected, rules] of [
    [files.filter((file) => !isFragment(file)), config],
    [files.filter(isFragment), { ...config, MD041: false }],
]) {
    // Fragments are short prose collated later, not standalone documents.
    if (selected.length) {
        Object.assign(results, await lint({ files: selected, config: rules }));
    }
}
const diagnostics = require("markdownlint/helpers").formatLintResults(results);
if (diagnostics.length) {
    console.error(diagnostics.join("\n"));
    process.exitCode = 1;
} else {
    console.log(`Markdownlint: ${files.length} files, no issues`);
}
