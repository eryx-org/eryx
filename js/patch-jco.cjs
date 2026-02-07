#!/usr/bin/env node
/**
 * Patches jco 1.16.1 generated JS for webpack compatibility and codegen bugs.
 * See ~/jco-1.16.1-bugs.md for detailed descriptions of each bug.
 */
const fs = require('fs');
const path = 'eryx-sandbox/eryx-sandbox.js';
let code = fs.readFileSync(path, 'utf8');
let patched = false;

// Patch 1: webpack compatibility for node:fs/promises
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
  code = code.replace(fsOriginal, fsPatched);
  console.log('Patched: webpack compatibility for node:fs/promises');
  patched = true;
} else if (code.includes('webpackIgnore')) {
  console.log('Already patched: webpack compatibility');
} else {
  console.error('Warning: Could not find fetchCompile pattern to patch');
}

// Patch 2: Fix jco codegen bug - 'for...in' should be 'for...of' in record lifting
const forInBug = 'for (const [key, liftFn, alignment32] in keysAndLiftFns)';
const forOfFix = 'for (const [key, liftFn, alignment32] of keysAndLiftFns)';
if (code.includes(forInBug)) {
  code = code.replace(forInBug, forOfFix);
  console.log('Patched: for...in -> for...of in _liftFlatRecordInner');
  patched = true;
}

// Patch 3: Fix jco codegen bug - bad variable references in _liftFlatStringUTF8
// The generated code references undefined 'params' and 'memory' variables,
// and incorrectly advances storagePtr by codeUnits instead of 8.
const stringLiftBug = `const start = new DataView(ctx.memory.buffer).getUint32(ctx.storagePtr, params[0], true);
    const codeUnits = new DataView(memory.buffer).getUint32(ctx.storagePtr, params[0] + 4, true);
    val = TEXT_DECODER_UTF8.decode(new Uint8Array(ctx.memory.buffer, start, codeUnits));
    ctx.storagePtr += codeUnits;
    ctx.storageLen -= codeUnits;`;
const stringLiftFix = `const start = new DataView(ctx.memory.buffer).getUint32(ctx.storagePtr, true);
    const codeUnits = new DataView(ctx.memory.buffer).getUint32(ctx.storagePtr + 4, true);
    val = TEXT_DECODER_UTF8.decode(new Uint8Array(ctx.memory.buffer, start, codeUnits));
    ctx.storagePtr += 8;
    ctx.storageLen -= 8;`;
if (code.includes(stringLiftBug)) {
  code = code.replace(stringLiftBug, stringLiftFix);
  console.log('Patched: _liftFlatStringUTF8 variable references and pointer advancement');
  patched = true;
}

// Patch 4: Fix _liftFlatRecordInner return value - should return [res, ctx] not just res
const recordReturnBugStr = `    return res;\n  }\n}`;
if (code.includes(recordReturnBugStr)) {
  code = code.replace(recordReturnBugStr, `    return [res, ctx];\n  }\n}`);
  console.log('Patched: _liftFlatRecordInner return [res, ctx]');
  patched = true;
}

// Patch 5: Fix const destructuring + reassignment in _liftFlatRecordInner
const constReassignBug = `    const { memory, useDirectParams, storagePtr, storageLen, params } = ctx;

    if (useDirectParams) {
      storagePtr = params[0]
    }`;
const constReassignFix = `    const { memory, useDirectParams, storagePtr, storageLen, params } = ctx;`;
if (code.includes(constReassignBug)) {
  code = code.replace(constReassignBug, constReassignFix);
  console.log('Patched: removed const reassignment in _liftFlatRecordInner');
  patched = true;
}

// Patch 6: Fix useDirectParams: false for result-returning async export trampolines
// The task.return for execute() passes flat values as direct params, not storage buffer ptrs
const useDirectBug = /useDirectParams: false,\n\s+getMemoryFn:/;
if (useDirectBug.test(code)) {
  code = code.replace(useDirectBug, 'useDirectParams: true,\n  getMemoryFn:');
  console.log('Patched: useDirectParams false -> true in taskReturn trampoline');
  patched = true;
}

if (patched) {
  fs.writeFileSync(path, code);
}
