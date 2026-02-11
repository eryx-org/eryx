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
    _linkerInit = import(/* @vite-ignore */ "./linker-wasm/linker");
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
  console.log("[native-extensions] transpile result imports:", transpiled.imports);
  console.log("[native-extensions] transpile result exports:", transpiled.exports);

  // Build an in-memory map of filename -> content
  const fileMap = new Map<string, Uint8Array>();
  let mainJsSource: string | null = null;

  for (const [name, content] of transpiled.files) {
    if (name.endsWith(".js")) {
      // Decode and apply patches to the main JS file
      let jsCode = new TextDecoder().decode(content);
      jsCode = applyJcoPatches(jsCode);

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

    // Load shim modules from the @bsull/eryx package
    const callbackShims: any = await import("@bsull/eryx/callbacks");
    const netShims: any = await import(/* @vite-ignore */ "@bsull/eryx/shims/net");
    const socketShims: any = await import(/* @vite-ignore */ "@bsull/eryx/shims/sockets");
    const [p2Cli, p2Clocks, p2Filesystem, p2Io, p2Random] =
      await Promise.all([
        import("@bytecodealliance/preview2-shim/cli"),
        import("@bytecodealliance/preview2-shim/clocks"),
        import("@bytecodealliance/preview2-shim/filesystem"),
        import("@bytecodealliance/preview2-shim/io"),
        import("@bytecodealliance/preview2-shim/random"),
      ]);

    // In instantiation mode, jco's instantiate(compileCore, imports) expects
    // imports keyed by module path (for mapped imports) or WIT interface name
    // (for unmapped ones). Since the map may not apply consistently, we
    // provide both styles and also discover keys from the transpiled output.
    const shimImports: Record<string, any> = {
      // Mapped custom shims (keyed by map target path)
      "./shims/callbacks.js": {
        invoke: callbackShims.invoke,
        listCallbacks: callbackShims.listCallbacks,
        reportTrace: callbackShims.reportTrace,
        reportOutput: callbackShims.reportOutput,
      },
      "./shims/net.js": {
        tcp: netShims.tcp,
        tls: netShims.tls,
      },
      "./shims/sockets.js": {
        instanceNetwork: socketShims.instanceNetwork,
        ipNameLookup: socketShims.ipNameLookup,
        network: socketShims.network,
        tcpCreateSocket: socketShims.tcpCreateSocket,
        tcp: socketShims.tcp,
        udpCreateSocket: socketShims.udpCreateSocket,
        udp: socketShims.udp,
      },
      // WASI imports keyed by individual WIT interface name.
      ...expandWasiImports("cli", p2Cli),
      ...expandWasiImports("clocks", p2Clocks),
      ...expandWasiImports("filesystem", p2Filesystem),
      ...expandWasiImports("io", p2Io),
      ...expandWasiImports("random", p2Random),
      // Socket shims also under WIT names (map may not apply for versioned keys)
      ...expandWasiImports("sockets", socketShims),
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

    // Set up environment and filesystem BEFORE instantiation — Py_InitializeEx
    // runs during __wasm_call_ctors and needs PYTHONHOME + stdlib available.
    onProgress("Loading Python stdlib...");
    await setupEnvironmentAndFilesystem();

    onProgress("Instantiating WASM (Python cold start, may take a while)...");
    const instance = await mod.instantiate(compileCore, shimImports);

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
 * Expand a preview2-shim sub-module into individual WIT interface entries.
 *
 * e.g. expandWasiImports("cli", p2Cli) produces:
 *   { "wasi:cli/environment": p2Cli.environment,
 *     "wasi:cli/exit": p2Cli.exit, ... }
 */
function expandWasiImports(
  namespace: string,
  mod: Record<string, any>,
): Record<string, any> {
  const result: Record<string, any> = {};
  for (const [key, val] of Object.entries(mod)) {
    if (key.startsWith("_") || typeof val !== "object") continue;
    const kebab = key.replace(/([a-z])([A-Z])/g, "$1-$2").toLowerCase();
    result[`wasi:${namespace}/${kebab}`] = val;
  }
  return result;
}

/**
 * Set up WASI environment variables and filesystem before Python initializes.
 *
 * Python needs PYTHONHOME and PYTHONPATH to find the stdlib, and the
 * preview2-shim filesystem must have the stdlib files available.
 */
async function setupEnvironmentAndFilesystem(): Promise<void> {
  // Set PYTHONHOME and PYTHONPATH so Py_InitializeEx can find the stdlib
  const cliMod: any = await import("@bytecodealliance/preview2-shim/cli");
  if (cliMod._setEnv) {
    cliMod._setEnv({
      PYTHONHOME: "/python-stdlib",
      PYTHONPATH: "/python-stdlib",
    });
  }

  // Load stdlib into the preview2-shim virtual filesystem
  const fsMod: any = await import(
    "@bytecodealliance/preview2-shim/filesystem"
  );

  // The main eryx module has already been loaded by the initial sandbox,
  // so _fileTree contains the stdlib + site-packages.
  const eryxMod = await import("@bsull/eryx");
  const fileTree = (eryxMod as any)._fileTree;
  if (!fileTree) {
    throw new Error("eryx _fileTree not available; initial sandbox must load first");
  }

  fsMod._setFileData(fileTree);
}

// --------------------------------------------------------------------------
// Public API
// --------------------------------------------------------------------------

/**
 * Detect native WASI extensions in a wheel's file entries.
 *
 * Returns entries whose filenames end in `.so` and contain `wasm32-wasi`.
 * The `name` field is set to the full installed filesystem path (e.g.,
 * `/site-packages/numpy/core/_multiarray_umath.cpython-314-wasm32-wasi.so`)
 * because wit-component's built-in libdl uses exact string matching in
 * `dlopen()` — the name must match what Python's import system passes.
 */
export function detectNativeExtensions(
  entries: [string, Uint8Array][],
): NativeExtension[] {
  const extensions: NativeExtension[] = [];
  for (const [path, data] of entries) {
    if (path.endsWith(".so") && path.includes("wasm32-wasi")) {
      // Use full installed path so dlopen() can find it by exact match
      const name = `/site-packages/${path}`;
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
