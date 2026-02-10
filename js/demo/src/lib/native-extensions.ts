/**
 * In-browser native WASM extension linking pipeline.
 *
 * When a user uploads a wheel containing native .so files (e.g., numpy),
 * this module orchestrates:
 *   1. Fetching + decompressing base libraries (libc, libpython, etc.)
 *   2. Linking extensions using the eryx-linker-wasm crate (wit-component)
 *   3. Transpiling the linked component via jco's browser API
 *   4. Applying jco 1.16.1 patches (ported from js/patch-jco.cjs)
 *   5. Dynamically loading the transpiled sandbox via blob URLs
 */

import type { DynamicSandboxExports } from "./sandbox.svelte";

// --------------------------------------------------------------------------
// Types
// --------------------------------------------------------------------------

export interface NativeExtension {
  name: string;
  bytes: Uint8Array;
}

export type ProgressCallback = (message: string) => void;

// --------------------------------------------------------------------------
// Lazy-loaded heavy dependencies (only fetched when needed)
// --------------------------------------------------------------------------

let _baseLibs: Map<string, Uint8Array> | null = null;
let _linkerInit: Promise<typeof import("./linker-wasm/linker")> | null = null;

const BASE_LIB_NAMES = [
  "libc.so.zst",
  "libc++.so.zst",
  "libc++abi.so.zst",
  "libpython3.14.so.zst",
  "libwasi-emulated-mman.so.zst",
  "libwasi-emulated-process-clocks.so.zst",
  "libwasi-emulated-getpid.so.zst",
  "libwasi-emulated-signal.so.zst",
  "wasi_snapshot_preview1.reactor.wasm.zst",
  "liberyx_runtime.so.zst",
  "liberyx_bindings.so.zst",
] as const;

// --------------------------------------------------------------------------
// Base library fetching + decompression
// --------------------------------------------------------------------------

async function fetchAndDecompressBaseLibs(
  onProgress: ProgressCallback,
): Promise<Map<string, Uint8Array>> {
  if (_baseLibs) return _baseLibs;

  onProgress("Fetching base libraries...");
  const { decompress } = await import("fzstd");

  const results = await Promise.all(
    BASE_LIB_NAMES.map(async (name) => {
      const resp = await fetch(`/libs/${name}`);
      if (!resp.ok)
        throw new Error(`Failed to fetch ${name}: ${resp.status}`);
      const compressed = new Uint8Array(await resp.arrayBuffer());
      const decompressed = decompress(compressed);
      // Strip .zst suffix for the map key
      return [name.replace(/\.zst$/, ""), decompressed] as const;
    }),
  );

  _baseLibs = new Map(results);
  return _baseLibs;
}

// --------------------------------------------------------------------------
// Linker WASM initialization
// --------------------------------------------------------------------------

function getLinker() {
  if (!_linkerInit) {
    _linkerInit = import("./linker-wasm/linker");
  }
  return _linkerInit;
}

async function linkExtensions(
  baseLibs: Map<string, Uint8Array>,
  extensions: NativeExtension[],
  onProgress: ProgressCallback,
): Promise<Uint8Array> {
  onProgress("Initializing linker...");
  const linker = await getLinker();
  // wasm-bindgen init
  await (linker as any).default();

  onProgress(`Linking ${extensions.length} extension(s)...`);

  const { NativeExtension: WasmNativeExtension, linkExtensions: link } =
    linker;

  const wasmExts = extensions.map(
    (ext) => new WasmNativeExtension(ext.name, ext.bytes),
  );

  const get = (name: string) => {
    const lib = baseLibs.get(name);
    if (!lib) throw new Error(`Missing base library: ${name}`);
    return lib;
  };

  return link(
    get("libc.so"),
    get("libc++.so"),
    get("libc++abi.so"),
    get("libpython3.14.so"),
    get("libwasi-emulated-mman.so"),
    get("libwasi-emulated-process-clocks.so"),
    get("libwasi-emulated-getpid.so"),
    get("libwasi-emulated-signal.so"),
    get("wasi_snapshot_preview1.reactor.wasm"),
    get("liberyx_runtime.so"),
    get("liberyx_bindings.so"),
    wasmExts,
  );
}

// --------------------------------------------------------------------------
// jco transpile + patching
// --------------------------------------------------------------------------

/** Import maps matching js/mise.toml:69-79. */
const JCO_MAP: [string, string][] = [
  ["eryx:net/tcp", "./shims/net.js#tcp"],
  ["eryx:net/tls", "./shims/net.js#tls"],
  ["invoke", "./shims/callbacks.js#invoke"],
  ["list-callbacks", "./shims/callbacks.js#listCallbacks"],
  ["report-trace", "./shims/callbacks.js#reportTrace"],
  ["report-output", "./shims/callbacks.js#reportOutput"],
  ["wasi:sockets/instance-network@0.2.3", "./shims/sockets.js#instanceNetwork"],
  ["wasi:sockets/ip-name-lookup@0.2.3", "./shims/sockets.js#ipNameLookup"],
  ["wasi:sockets/network@0.2.3", "./shims/sockets.js#network"],
  ["wasi:sockets/tcp-create-socket@0.2.3", "./shims/sockets.js#tcpCreateSocket"],
  ["wasi:sockets/tcp@0.2.3", "./shims/sockets.js#tcp"],
  ["wasi:sockets/udp-create-socket@0.2.3", "./shims/sockets.js#udpCreateSocket"],
  ["wasi:sockets/udp@0.2.3", "./shims/sockets.js#udp"],
];

interface TranspileResult {
  files: [string, Uint8Array][];
  imports: string[];
  exports: [string, string][];
}

async function transpileComponent(
  componentBytes: Uint8Array,
  onProgress: ProgressCallback,
): Promise<TranspileResult> {
  onProgress("Loading jco transpiler...");
  const jco = await import("@bytecodealliance/jco");
  // jco's browser build requires $init to complete (loads ~9 MB WASM)
  await (jco as any).$init;

  onProgress("Transpiling component...");
  const result = (jco as any).transpile(componentBytes, {
    name: "eryx-sandbox",
    instantiation: { tag: "async" },
    map: JCO_MAP,
    asyncMode: {
      tag: "jspi",
      val: {
        imports: ["invoke"],
        exports: [
          "execute",
          "snapshot-state",
          "restore-state",
          "clear-state",
        ],
      },
    },
  }) as TranspileResult;

  return result;
}

// --------------------------------------------------------------------------
// jco 1.16.1 patches (ported from js/patch-jco.cjs to in-memory transforms)
// --------------------------------------------------------------------------

function applyJcoPatches(code: string): string {
  // Patch 1: webpack compat for node:fs/promises (not needed for blob URLs,
  // but apply for consistency)
  {
    const fsOriginal = `  if (isNode) {
    _fs = _fs || await import('node:fs/promises');
    return WebAssembly.compile(await _fs.readFile(url));
  }`;
    const fsPatched = `  if (isNode) {
    if (!_fs) {
      try {
        _fs = await import('node:fs/promises');
      } catch {}
    }
    if (_fs) {
      return WebAssembly.compile(await _fs.readFile(url));
    }
  }`;
    code = code.replace(fsOriginal, fsPatched);
  }

  // Patch 2: for...in -> for...of in record lifting
  code = code.replace(
    "for (const [key, liftFn, alignment32] in keysAndLiftFns)",
    "for (const [key, liftFn, alignment32] of keysAndLiftFns)",
  );

  // Patch 3: Fix _liftFlatStringUTF8 variable references
  {
    const bug = `const start = new DataView(ctx.memory.buffer).getUint32(ctx.storagePtr, params[0], true);
    const codeUnits = new DataView(memory.buffer).getUint32(ctx.storagePtr, params[0] + 4, true);
    val = TEXT_DECODER_UTF8.decode(new Uint8Array(ctx.memory.buffer, start, codeUnits));
    ctx.storagePtr += codeUnits;
    ctx.storageLen -= codeUnits;`;
    const fix = `const start = new DataView(ctx.memory.buffer).getUint32(ctx.storagePtr, true);
    const codeUnits = new DataView(ctx.memory.buffer).getUint32(ctx.storagePtr + 4, true);
    val = TEXT_DECODER_UTF8.decode(new Uint8Array(ctx.memory.buffer, start, codeUnits));
    ctx.storagePtr += 8;
    ctx.storageLen -= 8;`;
    code = code.replace(bug, fix);
  }

  // Patch 4: Fix _liftFlatRecordInner return value
  {
    const bug = `    return res;\n  }\n}`;
    code = code.replace(bug, `    return [res, ctx];\n  }\n}`);
  }

  // Patch 5: Fix const destructuring + reassignment in _liftFlatRecordInner
  {
    const re =
      /const \{ memory, useDirectParams, storagePtr, storageLen, params \} = ctx;\s+if \(useDirectParams\) \{\s+storagePtr = params\[0\]\s+\}/;
    code = code.replace(
      re,
      `const { memory, useDirectParams, storagePtr, storageLen, params } = ctx;`,
    );
  }

  // Patch 6: Fix useDirectParams: false -> true in taskReturn trampoline
  {
    const re = /useDirectParams: false,\n\s+getMemoryFn:/;
    code = code.replace(re, "useDirectParams: true,\n  getMemoryFn:");
  }

  // Patch 7: Fix task-return map using raw WASM exports instead of lifting trampolines
  {
    const executeTramp = code.match(
      /const (trampoline\d+) = taskReturn\.bind\([^;]*?'stdout'[^;]*?\);/s,
    )?.[1];
    const snapshotTramp = code.match(
      /const (trampoline\d+) = taskReturn\.bind\([^;]*?_liftFlatList[^;]*?\);/s,
    )?.[1];
    const restoreTramp = code.match(
      /const (trampoline\d+) = taskReturn\.bind\([^;]*?'ok', null, null[^;]*?\);/s,
    )?.[1];

    if (executeTramp && snapshotTramp && restoreTramp) {
      const re =
        /'\[task-return\](execute|snapshot-state|restore-state)':\s*exports0\['\d+'\]/g;
      code = code.replace(re, (match: string, name: string) => {
        switch (name) {
          case "execute":
            return `'[task-return]execute': ${executeTramp}`;
          case "snapshot-state":
            return `'[task-return]snapshot-state': ${snapshotTramp}`;
          case "restore-state":
            return `'[task-return]restore-state': ${restoreTramp}`;
          default:
            return match;
        }
      });
    }
  }

  // Patch 9: Fix _liftFlatList for list<u8> in snapshot-state result lifting
  {
    const bug =
      "_liftFlatResult([['ok', _liftFlatList.bind(null, 4), 8]";
    const fix =
      "_liftFlatResult([['ok', function(ctx){const[p,c]=_liftFlatU32(ctx);const[l,c2]=_liftFlatU32(c);return[new Uint8Array(c2.memory.buffer.slice(p,p+l)),c2];}, 8]";
    code = code.replace(bug, fix);
  }

  return code;
}

// --------------------------------------------------------------------------
// Dynamic sandbox loading via blob URLs
// --------------------------------------------------------------------------

async function loadDynamicSandbox(
  transpiled: TranspileResult,
  onProgress: ProgressCallback,
): Promise<DynamicSandboxExports> {
  onProgress("Loading dynamic sandbox...");

  // Build an in-memory map of filename -> content
  const fileMap = new Map<string, Uint8Array>();
  let mainJsSource: string | null = null;

  for (const [name, content] of transpiled.files) {
    if (name.endsWith(".js")) {
      // Decode and apply patches to the main JS file
      let jsCode = new TextDecoder().decode(content);
      jsCode = applyJcoPatches(jsCode);

      // Rewrite import paths for shims to use absolute URLs from the eryx package.
      // The transpiled code imports from relative paths like './shims/net.js'
      // which won't resolve from a blob URL. Rewrite them to absolute module paths.
      jsCode = rewriteShimImports(jsCode);

      mainJsSource = jsCode;
    } else {
      fileMap.set(name, content);
    }
  }

  if (!mainJsSource) {
    throw new Error("No JS file found in transpiled output");
  }

  // Create blob URLs for WASM files
  const wasmBlobUrls = new Map<string, string>();
  for (const [name, content] of fileMap) {
    if (name.endsWith(".wasm")) {
      const blob = new Blob([content as BlobPart], { type: "application/wasm" });
      wasmBlobUrls.set(name, URL.createObjectURL(blob));
    }
  }

  // Rewrite WASM file references in the JS source to use blob URLs
  for (const [name, blobUrl] of wasmBlobUrls) {
    // jco generates code like: compileCore('eryx-sandbox.core.wasm')
    mainJsSource = mainJsSource.replaceAll(`'${name}'`, `'${blobUrl}'`);
    mainJsSource = mainJsSource.replaceAll(`"${name}"`, `"${blobUrl}"`);
  }

  // Create blob URL for the JS module
  const jsBlob = new Blob([mainJsSource], {
    type: "application/javascript",
  });
  const jsBlobUrl = URL.createObjectURL(jsBlob);

  try {
    const mod = await import(/* @vite-ignore */ jsBlobUrl);

    // With instantiation: { tag: 'async' }, jco generates an instantiate() function
    // that takes (compileCore, imports).
    if (typeof mod.instantiate !== "function") {
      throw new Error(
        "Transpiled module does not export instantiate(). " +
          "Ensure jco transpile used instantiation: { tag: 'async' }.",
      );
    }

    onProgress("Instantiating WASM...");

    // Load shim modules for the imports
    // Import shim modules. Use @vite-ignore for dynamic paths that Vite
    // shouldn't try to resolve at build time.
    const callbackShims: any = await import(
      /* @vite-ignore */ new URL(
        "../eryx/shims/callbacks.js",
        import.meta.url,
      ).href
    );
    const netShims: any = await import(
      /* @vite-ignore */ new URL(
        "../eryx/shims/net.js",
        import.meta.url,
      ).href
    );
    const socketShims: any = await import(
      /* @vite-ignore */ new URL(
        "../eryx/shims/sockets.js",
        import.meta.url,
      ).href
    );
    const [p2Cli, p2Clocks, p2Filesystem, p2Io, p2Random] =
      await Promise.all([
        import("@bytecodealliance/preview2-shim/cli"),
        import("@bytecodealliance/preview2-shim/clocks"),
        import("@bytecodealliance/preview2-shim/filesystem"),
        import("@bytecodealliance/preview2-shim/io"),
        import("@bytecodealliance/preview2-shim/random"),
      ]);

    // Build the imports object matching what the instantiation mode expects.
    // The import names come from the --map flags and preview2-shim defaults.
    const shimImports: Record<string, any> = {
      "eryx:net/tcp": netShims.tcp,
      "eryx:net/tls": netShims.tls,
      invoke: callbackShims.invoke,
      "list-callbacks": callbackShims.listCallbacks,
      "report-trace": callbackShims.reportTrace,
      "report-output": callbackShims.reportOutput,
      "wasi:sockets/instance-network@0.2.3": socketShims.instanceNetwork,
      "wasi:sockets/ip-name-lookup@0.2.3": socketShims.ipNameLookup,
      "wasi:sockets/network@0.2.3": socketShims.network,
      "wasi:sockets/tcp-create-socket@0.2.3": socketShims.tcpCreateSocket,
      "wasi:sockets/tcp@0.2.3": socketShims.tcp,
      "wasi:sockets/udp-create-socket@0.2.3": socketShims.udpCreateSocket,
      "wasi:sockets/udp@0.2.3": socketShims.udp,
      // preview2-shim WASI imports
      ...extractWasiImports("cli", p2Cli),
      ...extractWasiImports("clocks", p2Clocks),
      ...extractWasiImports("filesystem", p2Filesystem),
      ...extractWasiImports("io", p2Io),
      ...extractWasiImports("random", p2Random),
    };

    // compileCore: resolves WASM URLs to compiled modules.
    // Blob URLs don't support compileStreaming, so use arrayBuffer fallback.
    const compileCore = async (url: string) => {
      const resp = await fetch(url);
      if (url.startsWith("blob:")) {
        const bytes = await resp.arrayBuffer();
        return WebAssembly.compile(bytes);
      }
      return WebAssembly.compileStreaming(resp);
    };

    const instance = await mod.instantiate(compileCore, shimImports);

    // Set up filesystem before finalizePreinit
    onProgress("Loading Python stdlib...");
    await setupFilesystem();

    onProgress("Initializing Python interpreter (this may take a while)...");
    instance.finalizePreinit();

    return {
      execute: instance.execute,
      snapshotState: instance.snapshotState,
      restoreState: instance.restoreState,
      clearState: instance.clearState,
      finalizePreinit: instance.finalizePreinit,
    };
  } finally {
    URL.revokeObjectURL(jsBlobUrl);
    for (const url of wasmBlobUrls.values()) {
      URL.revokeObjectURL(url);
    }
  }
}

/**
 * Extract WASI imports from a preview2-shim sub-module.
 *
 * Each sub-module (e.g., "cli") exports named members like `environment`,
 * `exit`, `stderr`, etc. These map to `wasi:cli/environment@0.2.3`, etc.
 */
function extractWasiImports(
  namespace: string,
  mod: any,
): Record<string, any> {
  const imports: Record<string, any> = {};
  for (const [key, val] of Object.entries(mod)) {
    if (key.startsWith("_")) continue; // Skip private exports
    const wasiKey = `wasi:${namespace}/${camelToKebab(key)}@0.2.3`;
    imports[wasiKey] = val;
  }
  return imports;
}

function camelToKebab(s: string): string {
  return s.replace(/([a-z])([A-Z])/g, "$1-$2").toLowerCase();
}

/**
 * Rewrite shim import paths in transpiled JS to absolute URLs.
 *
 * jco generates imports like `from './shims/net.js'` which won't resolve
 * from a blob URL context. With instantiation: 'async', most imports are
 * passed via the imports object, but some may still be referenced.
 */
function rewriteShimImports(code: string): string {
  // In instantiation mode, imports are passed to instantiate(),
  // so we shouldn't need to rewrite imports. But if there are
  // any static imports of preview2-shim, handle them.
  return code;
}

/** Set up the preview2-shim filesystem with stdlib. */
async function setupFilesystem(): Promise<void> {
  const fsMod: any = await import(
    "@bytecodealliance/preview2-shim/filesystem"
  );
  const _setFileData = fsMod._setFileData;

  // The main eryx module has already been loaded by the initial sandbox,
  // so _fileTree contains the stdlib + site-packages.
  const eryxMod = await import("@bsull/eryx");
  const fileTree = (eryxMod as any)._fileTree;
  if (!fileTree) {
    throw new Error("eryx _fileTree not available; initial sandbox must load first");
  }

  _setFileData(fileTree);
}

// --------------------------------------------------------------------------
// Public API
// --------------------------------------------------------------------------

/**
 * Detect native WASI extensions in a wheel's file entries.
 *
 * Returns entries whose filenames end in `.so` and contain `wasm32-wasi`.
 */
export function detectNativeExtensions(
  entries: [string, Uint8Array][],
): NativeExtension[] {
  const extensions: NativeExtension[] = [];
  for (const [path, data] of entries) {
    if (path.endsWith(".so") && path.includes("wasm32-wasi")) {
      const name = path.split("/").pop() || path;
      extensions.push({ name, bytes: data });
    }
  }
  return extensions;
}

/**
 * Full pipeline: link native extensions, transpile, and load a new sandbox.
 *
 * Returns the dynamic sandbox exports ready for use.
 */
export async function linkAndTranspile(
  extensions: NativeExtension[],
  onProgress: ProgressCallback,
): Promise<DynamicSandboxExports> {
  // Step 1: Fetch + decompress base libraries
  const baseLibs = await fetchAndDecompressBaseLibs(onProgress);

  // Step 2: Link extensions with base libraries
  const componentBytes = await linkExtensions(
    baseLibs,
    extensions,
    onProgress,
  );

  // Step 3: Transpile the linked component
  const transpiled = await transpileComponent(componentBytes, onProgress);

  // Step 4: Load the dynamic sandbox
  return loadDynamicSandbox(transpiled, onProgress);
}
