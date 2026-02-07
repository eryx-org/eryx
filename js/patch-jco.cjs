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
// The blank line between const and if may have trailing whitespace, so use regex
const constReassignBug = /const \{ memory, useDirectParams, storagePtr, storageLen, params \} = ctx;\s+if \(useDirectParams\) \{\s+storagePtr = params\[0\]\s+\}/;
const constReassignFix = `const { memory, useDirectParams, storagePtr, storageLen, params } = ctx;`;
if (constReassignBug.test(code)) {
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

// Patch 7: Fix task-return map using raw WASM exports instead of lifting trampolines
// jco generates proper taskReturn.bind trampolines with liftFns for async export
// results, but wires raw WASM exports into the task-return map instead.
// This causes "liftFn is not a function" errors when snapshot-state/restore-state return.
{
  // Find the three taskReturn trampolines by their distinctive lift patterns
  const executeTramp = code.match(/const (trampoline\d+) = taskReturn\.bind\([^;]*?'stdout'[^;]*?\);/s)?.[1];
  const snapshotTramp = code.match(/const (trampoline\d+) = taskReturn\.bind\([^;]*?_liftFlatList[^;]*?\);/s)?.[1];
  const restoreTramp = code.match(/const (trampoline\d+) = taskReturn\.bind\([^;]*?'ok', null, null[^;]*?\);/s)?.[1];

  if (executeTramp && snapshotTramp && restoreTramp) {
    let count = 0;
    const taskReturnExportRe = /'\[task-return\](execute|snapshot-state|restore-state)':\s*exports0\['\d+'\]/g;
    code = code.replace(taskReturnExportRe, (match, name) => {
      count++;
      switch (name) {
        case 'execute': return `'[task-return]execute': ${executeTramp}`;
        case 'snapshot-state': return `'[task-return]snapshot-state': ${snapshotTramp}`;
        case 'restore-state': return `'[task-return]restore-state': ${restoreTramp}`;
        default: return match;
      }
    });
    if (count > 0) {
      console.log(`Patched: wired ${count} task-return trampolines (${executeTramp}, ${snapshotTramp}, ${restoreTramp})`);
      patched = true;
    }
  } else if (code.match(/'\[task-return\]execute':\s*trampoline\d+/)) {
    console.log('Already patched: task-return trampolines');
  } else {
    console.error('Warning: Could not find task-return trampolines to wire');
  }
}

// Patch 9: Fix _liftFlatList for list<u8> in snapshot-state result lifting
// _liftFlatList has two bugs: (1) missing return _liftFlatListInner, and
// (2) .bind(null, 4) passes 4 as elemLiftFn instead of alignment32.
// Replace with an inline list<u8> lifter that reads (ptr, len) from params.
{
  const listLiftBug = "_liftFlatResult([['ok', _liftFlatList.bind(null, 4), 8]";
  const listLiftFix = "_liftFlatResult([['ok', function(ctx){const[p,c]=_liftFlatU32(ctx);const[l,c2]=_liftFlatU32(c);return[new Uint8Array(c2.memory.buffer.slice(p,p+l)),c2];}, 8]";
  if (code.includes(listLiftBug)) {
    code = code.replace(listLiftBug, listLiftFix);
    console.log('Patched: replaced _liftFlatList.bind(null, 4) with inline list<u8> lifter');
    patched = true;
  } else if (code.includes('function(ctx){const[p,c]=_liftFlatU32')) {
    console.log('Already patched: list<u8> lifter');
  } else {
    console.error('Warning: Could not find _liftFlatList.bind(null, 4) in result to patch');
  }
}

if (patched) {
  fs.writeFileSync(path, code);
}

// Patch 8: Strip debug console.log statements from preview2-shim filesystem.js
// These produce noisy [filesystem] FLAGS FOR, RENAME AT, etc. messages
const shimFsPath = 'eryx-sandbox/node_modules/@bytecodealliance/preview2-shim/lib/browser/filesystem.js';
if (fs.existsSync(shimFsPath)) {
  let shimCode = fs.readFileSync(shimFsPath, 'utf8');
  const debugLogRe = /^\s*console\.log\(`\[filesystem\].*$\n?/gm;
  if (debugLogRe.test(shimCode)) {
    shimCode = shimCode.replace(debugLogRe, '');
    fs.writeFileSync(shimFsPath, shimCode);
    console.log('Patched: stripped [filesystem] debug logs from preview2-shim');
  } else {
    console.log('Already patched: preview2-shim filesystem debug logs');
  }
}
