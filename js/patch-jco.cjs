#!/usr/bin/env node
/**
 * Patches jco-generated JS for webpack compatibility.
 *
 * Earlier jco versions needed codegen fixes here (record/list/string lifting,
 * task-return wiring, kebab-case record keys); jco 1.35 generates all of those
 * correctly, and preview2-shim 0.26 no longer ships [filesystem] debug logs.
 *
 * Fails if the target pattern is missing so a jco bump can't silently drift.
 */
const fs = require('fs');

// webpack tries to bundle the dynamic node:fs/promises import in fetchWithNode
// and fails; mark it webpackIgnore and fall back to fetch when unavailable.
const fsOriginal = `	if (context.fileCache.node) {
		context.fileCache.fs = context.fileCache.fs || await import("node:fs/promises");
		return context.WebAssembly.compile(await context.fileCache.fs.readFile(url));
	}`;

const fsPatched = `	if (context.fileCache.node) {
		if (!context.fileCache.fs) {
			try {
				context.fileCache.fs = await import(/* webpackIgnore: true */ "node:fs/promises");
			} catch {
				// Fallback for environments where node:fs/promises is unavailable
			}
		}
		if (context.fileCache.fs) {
			return context.WebAssembly.compile(await context.fileCache.fs.readFile(url));
		}
	}`;

// One per guest language, and the shell's bundled coreutils.
for (const path of [
  'eryx/eryx-sandbox.js',
  'eryx/eryx-sandbox-javascript.js',
  'eryx/eryx-sandbox-shell.js',
  'eryx/eryx-coreutils.js',
]) {
  const code = fs.readFileSync(path, 'utf8');
  if (code.includes(fsOriginal)) {
    fs.writeFileSync(path, code.replace(fsOriginal, fsPatched));
    console.log(`Patched ${path}: webpack compatibility for node:fs/promises`);
  } else if (code.includes('webpackIgnore')) {
    console.log(`Already patched ${path}: webpack compatibility`);
  } else {
    console.error(`Error: could not find fetchCompile pattern to patch in ${path}; jco output changed`);
    process.exit(1);
  }
}
