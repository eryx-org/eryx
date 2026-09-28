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
const path = 'eryx/eryx-sandbox.js';
let code = fs.readFileSync(path, 'utf8');

// webpack tries to bundle the dynamic node:fs/promises import in fetchCompile
// and fails; mark it webpackIgnore and fall back to fetch when unavailable.
const fsOriginal = `  if (isNode) {
    _fs = _fs || await import('node:fs/promises');
    return WebAssembly.compile(await _fs.readFile(url));
  }`;

const fsPatched = `  if (isNode) {
    if (!_fs) {
      try {
        _fs = await import(/* webpackIgnore: true */ 'node:fs/promises');
      } catch {
        // Fallback for environments where node:fs/promises is unavailable
      }
    }
    if (_fs) {
      return WebAssembly.compile(await _fs.readFile(url));
    }
  }`;

if (code.includes(fsOriginal)) {
  fs.writeFileSync(path, code.replace(fsOriginal, fsPatched));
  console.log('Patched: webpack compatibility for node:fs/promises');
} else if (code.includes('webpackIgnore')) {
  console.log('Already patched: webpack compatibility');
} else {
  console.error('Error: could not find fetchCompile pattern to patch; jco output changed');
  process.exit(1);
}
