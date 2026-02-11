<script lang="ts">
  import { getSandboxState, replaceSandbox, runCode } from "../lib/sandbox.svelte";
  import { detectNativeExtensions, linkAndTranspile } from "../lib/native-extensions";
  import type { NativeExtension } from "../lib/native-extensions";
  import CodeEditor from "./CodeEditor.svelte";
  import OutputBox from "./OutputBox.svelte";

  type PackageStatus = "pending" | "installing" | "installed" | "linking" | "error";

  interface Package {
    name: string;
    version: string;
    size: number;
    file: File;
    status: PackageStatus;
    fileCount?: number;
    nativeExtCount?: number;
    error?: string;
  }

  let packages: Package[] = $state([]);
  let dragover = $state(false);
  let sitePackagesConfigured = false;
  let statusMessage: string | null = $state(null);
  let statusType: "success" | "error" = $state("success");
  let linkProgress: string | null = $state(null);

  let state = $derived(getSandboxState());

  const EXAMPLES = [
    {
      filename: "humanize-4.15.0-py3-none-any.whl",
      label: "humanize",
      meta: "132 KB · pure Python",
      code: 'import humanize\nprint(humanize.naturalsize(1048576))',
    },
    {
      filename: "numpy-wasi.tar.gz",
      label: "numpy",
      meta: "9.3 MB · native ext",
      code: 'import numpy\nprint(numpy.sum([1, 2, 3]))',
    },
  ] as const;

  let exampleLoading: string | null = $state(null);

  async function installExample(ex: (typeof EXAMPLES)[number]) {
    if (exampleLoading) return;
    exampleLoading = ex.filename;
    try {
      const resp = await fetch(`${import.meta.env.BASE_URL}examples/${ex.filename}`);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const ct = resp.headers.get("content-type") || "";
      if (ct.startsWith("text/html"))
        throw new Error("Example file not found (got HTML fallback)");
      const blob = await resp.blob();
      const file = new File([blob], ex.filename, { type: blob.type });
      const dt = new DataTransfer();
      dt.items.add(file);
      await handleFiles(dt.files);
      code = ex.code;
    } catch (e) {
      showStatus(`Failed to load ${ex.label}: ${(e as Error).message}`, "error");
    } finally {
      exampleLoading = null;
    }
  }

  let code = $state(`import example_pkg\nprint(example_pkg)`);
  let output: string | null = $state(null);
  let isError = $state(false);
  let elapsed: number | null = $state(null);

  async function run() {
    const trimmed = code.trim();
    if (!trimmed) return;
    const result = await runCode(trimmed);
    if (result.ok) {
      output = result.result.stdout || result.result.stderr || "(no output)";
      isError = false;
    } else {
      output = "Error: " + result.error;
      isError = true;
    }
    elapsed = result.elapsed;
  }

  // Lazily loaded references to the filesystem shim and file tree
  let _setFileData: ((data: any) => void) | null = null;
  let fileTree: { dir: Record<string, any> } | null = null;

  async function ensureFileTreeLoaded() {
    if (fileTree) return;
    const [shimMod, eryxMod] = await Promise.all([
      import("@bytecodealliance/preview2-shim/filesystem"),
      import("@bsull/eryx"),
    ]);
    _setFileData = shimMod._setFileData;
    fileTree = (eryxMod as any)._fileTree;
  }

  function showStatus(msg: string, type: "success" | "error" = "success") {
    statusMessage = msg;
    statusType = type;
    setTimeout(() => (statusMessage = null), 4000);
  }

  function isSupportedPackage(name: string): boolean {
    return name.endsWith(".whl") || name.endsWith(".tar.gz") || name.endsWith(".tgz");
  }

  function parsePackageName(filename: string): { name: string; version: string } {
    if (filename.endsWith(".whl")) {
      const parts = filename.replace(".whl", "").split("-");
      return { name: parts[0], version: parts[1] || "?" };
    }
    // tar.gz: e.g. "numpy-wasi.tar.gz" or "numpy-1.26.4.tar.gz"
    const base = filename.replace(/\.tar\.gz$|\.tgz$/, "");
    const match = base.match(/^(.+?)-(\d+\..*)$/);
    if (match) return { name: match[1], version: match[2] };
    return { name: base, version: "?" };
  }

  async function handleFiles(files: FileList) {
    for (const f of files) {
      if (!isSupportedPackage(f.name)) continue;
      const { name, version } = parsePackageName(f.name);
      packages.push({
        name,
        version,
        size: f.size,
        file: f,
        status: "pending",
      });
    }
    packages = [...packages];

    await ensureFileTreeLoaded();

    // Install each pending package
    for (const pkg of packages) {
      if (pkg.status !== "pending") continue;
      pkg.status = "installing";
      packages = [...packages];
      try {
        await installPackage(pkg);
        pkg.status = "installed";
      } catch (e) {
        pkg.status = "error";
        pkg.error = (e as Error).message;
        console.error(`Failed to install ${pkg.name}:`, e);
      }
      packages = [...packages];
    }

    // Configure sys.path if we have any installed packages
    if (
      !sitePackagesConfigured &&
      packages.some((p) => p.status === "installed")
    ) {
      sitePackagesConfigured = true;
      await runCode('import sys; sys.path.insert(0, "/site-packages")');
    }

    const installed = packages.filter((p) => p.status === "installed");
    if (installed.length > 0) {
      const latest = installed[installed.length - 1];
      code = `import ${latest.name}\nprint(${latest.name})`;
      showStatus(
        `${installed.length} package${installed.length > 1 ? "s" : ""} installed`,
      );
    }
  }

  async function installPackage(pkg: Package) {
    const arrayBuffer = await pkg.file.arrayBuffer();
    const raw = new Uint8Array(arrayBuffer);
    const entries = pkg.file.name.endsWith(".whl")
      ? await parseZip(raw)
      : await parseTarGz(raw);

    // Check for native WASI extensions
    const nativeExts = detectNativeExtensions(entries);
    if (nativeExts.length > 0) {
      pkg.nativeExtCount = nativeExts.length;
      await installWithNativeExtensions(pkg, entries, nativeExts);
      return;
    }

    // Pure Python wheel: install directly into the filesystem
    const sitePackages = fileTree!.dir["site-packages"];
    let fileCount = 0;

    for (const [path, data] of entries) {
      // Skip .dist-info metadata directories and __pycache__
      if (path.includes(".dist-info/") || path.includes("__pycache__/"))
        continue;

      const parts = path.split("/");
      let current = sitePackages;
      for (let i = 0; i < parts.length - 1; i++) {
        if (!parts[i]) continue;
        if (!current.dir) current.dir = {};
        if (!current.dir[parts[i]]) current.dir[parts[i]] = { dir: {} };
        current = current.dir[parts[i]];
      }
      const fileName = parts[parts.length - 1];
      if (fileName) {
        if (!current.dir) current.dir = {};
        current.dir[fileName] = { source: data };
        fileCount++;
      }
    }

    pkg.fileCount = fileCount;
    _setFileData!(fileTree);
  }

  async function installWithNativeExtensions(
    pkg: Package,
    entries: [string, Uint8Array][],
    nativeExts: NativeExtension[],
  ) {
    pkg.status = "linking";
    packages = [...packages];

    await replaceSandbox(async (onProgress) => {
      linkProgress = "Starting native extension linking...";

      const exports = await linkAndTranspile(nativeExts, (msg) => {
        linkProgress = msg;
        onProgress(msg);
      });

      // After sandbox is replaced, install Python files into the new filesystem
      linkProgress = "Installing Python files...";
      await ensureFileTreeLoaded();

      const sitePackages = fileTree!.dir["site-packages"];
      let fileCount = 0;

      for (const [path, data] of entries) {
        if (path.includes(".dist-info/") || path.includes("__pycache__/"))
          continue;
        // Keep .so files on the filesystem — Python's import system needs to
        // find them via stat/readdir before it'll call dlopen(). The actual
        // native code is already linked into the WASM component; dlopen()
        // resolves pre-linked libraries by name, not by file content.

        const parts = path.split("/");
        let current = sitePackages;
        for (let i = 0; i < parts.length - 1; i++) {
          if (!parts[i]) continue;
          if (!current.dir) current.dir = {};
          if (!current.dir[parts[i]]) current.dir[parts[i]] = { dir: {} };
          current = current.dir[parts[i]];
        }
        const fileName = parts[parts.length - 1];
        if (fileName) {
          if (!current.dir) current.dir = {};
          current.dir[fileName] = { source: data };
          fileCount++;
        }
      }

      pkg.fileCount = fileCount;
      _setFileData!(fileTree);

      linkProgress = null;
      return exports;
    });

    // Configure sys.path for the new sandbox
    sitePackagesConfigured = false;
  }

  /**
   * Parse a .tar.gz file and return [path, Uint8Array] entries.
   */
  async function parseTarGz(
    data: Uint8Array,
  ): Promise<[string, Uint8Array][]> {
    // Decompress gzip (skip if already decompressed, e.g. by Content-Encoding)
    let tar: Uint8Array;
    const isGzip = data.length >= 2 && data[0] === 0x1f && data[1] === 0x8b;
    if (isGzip) {
      const ds = new DecompressionStream("gzip");
      const writer = ds.writable.getWriter();
      const reader = ds.readable.getReader();
      writer.write(data);
      writer.close();
      const chunks: Uint8Array[] = [];
      let totalLen = 0;
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        chunks.push(value);
        totalLen += value.length;
      }
      tar = new Uint8Array(totalLen);
      let writeOffset = 0;
      for (const chunk of chunks) {
        tar.set(chunk, writeOffset);
        writeOffset += chunk.length;
      }
    } else {
      tar = data;
    }

    // Parse tar
    const entries: [string, Uint8Array][] = [];
    let pos = 0;
    const decoder = new TextDecoder();

    while (pos + 512 <= tar.length) {
      const header = tar.subarray(pos, pos + 512);
      // Check for end-of-archive (two zero blocks)
      if (header.every((b) => b === 0)) break;

      const nameRaw = decoder.decode(header.subarray(0, 100)).replace(/\0.*/, "");
      const sizeOctal = decoder.decode(header.subarray(124, 136)).replace(/\0.*/, "").trim();
      const typeFlag = header[156];
      const prefix = decoder.decode(header.subarray(345, 500)).replace(/\0.*/, "");

      const fullName = prefix ? `${prefix}/${nameRaw}` : nameRaw;
      const size = sizeOctal ? parseInt(sizeOctal, 8) : 0;

      pos += 512; // Move past header

      if (typeFlag === 0x30 || typeFlag === 0) {
        // Regular file
        if (size > 0 && !fullName.endsWith("/")) {
          entries.push([fullName, tar.slice(pos, pos + size)]);
        }
      }

      // Advance past file data (rounded up to 512-byte blocks)
      pos += Math.ceil(size / 512) * 512;
    }

    return entries;
  }

  /**
   * Parse a ZIP file (wheel) and return [path, Uint8Array] entries.
   */
  async function parseZip(
    data: Uint8Array,
  ): Promise<[string, Uint8Array][]> {
    const entries: [string, Uint8Array][] = [];
    const view = new DataView(data.buffer, data.byteOffset, data.byteLength);

    // Find End of Central Directory record (scan backwards)
    let eocdOffset = -1;
    for (let i = data.length - 22; i >= 0; i--) {
      if (view.getUint32(i, true) === 0x06054b50) {
        eocdOffset = i;
        break;
      }
    }
    if (eocdOffset === -1) throw new Error("Not a valid ZIP file");

    const cdOffset = view.getUint32(eocdOffset + 16, true);
    const cdCount = view.getUint16(eocdOffset + 10, true);
    let pos = cdOffset;

    for (let i = 0; i < cdCount; i++) {
      if (view.getUint32(pos, true) !== 0x02014b50) break;

      const compressionMethod = view.getUint16(pos + 10, true);
      const compressedSize = view.getUint32(pos + 20, true);
      const uncompressedSize = view.getUint32(pos + 24, true);
      const nameLen = view.getUint16(pos + 28, true);
      const extraLen = view.getUint16(pos + 30, true);
      const commentLen = view.getUint16(pos + 32, true);
      const localHeaderOffset = view.getUint32(pos + 42, true);
      const name = new TextDecoder().decode(
        data.subarray(pos + 46, pos + 46 + nameLen),
      );

      pos += 46 + nameLen + extraLen + commentLen;

      // Skip directories
      if (name.endsWith("/")) continue;

      // Read from local file header
      const localNameLen = view.getUint16(localHeaderOffset + 26, true);
      const localExtraLen = view.getUint16(localHeaderOffset + 28, true);
      const dataStart = localHeaderOffset + 30 + localNameLen + localExtraLen;

      let fileData: Uint8Array;
      if (compressionMethod === 0) {
        // Stored (no compression)
        fileData = data.slice(dataStart, dataStart + uncompressedSize);
      } else if (compressionMethod === 8) {
        // Deflate
        const compressed = data.subarray(
          dataStart,
          dataStart + compressedSize,
        );
        const ds = new DecompressionStream("deflate-raw");
        const writer = ds.writable.getWriter();
        const reader = ds.readable.getReader();
        writer.write(compressed);
        writer.close();
        const chunks: Uint8Array[] = [];
        let totalLen = 0;
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          chunks.push(value);
          totalLen += value.length;
        }
        fileData = new Uint8Array(totalLen);
        let offset = 0;
        for (const chunk of chunks) {
          fileData.set(chunk, offset);
          offset += chunk.length;
        }
      } else {
        console.warn(
          `Skipping ${name}: unsupported compression method ${compressionMethod}`,
        );
        continue;
      }

      entries.push([name, fileData]);
    }

    return entries;
  }

  function handleDrop(e: DragEvent) {
    e.preventDefault();
    dragover = false;
    if (e.dataTransfer?.files) handleFiles(e.dataTransfer.files);
  }

  let fileInput: HTMLInputElement;
</script>

<p class="hint">
  Upload Python packages (.whl or .tar.gz) to make them available for import.
  Pure-Python packages are extracted into a virtual filesystem. Packages with
  native WASI extensions trigger an in-browser linking and transpilation
  pipeline, which may take a minute or two.
</p>

{#if statusMessage}
  <div class="status" class:success={statusType === "success"} class:error={statusType === "error"}>
    {statusMessage}
  </div>
{/if}

{#if linkProgress}
  <div class="link-progress">
    <div class="spinner"></div>
    <span>{linkProgress}</span>
  </div>
{/if}

<div
  class="upload-zone"
  class:dragover
  role="button"
  tabindex="0"
  onclick={() => fileInput.click()}
  onkeydown={(e) => e.key === "Enter" && fileInput.click()}
  ondragover={(e) => {
    e.preventDefault();
    dragover = true;
  }}
  ondragleave={() => (dragover = false)}
  ondrop={handleDrop}
>
  <p><strong>Drop .whl or .tar.gz files here</strong> or click to browse</p>
  <p class="small">Pure Python and native WASI extension packages are supported</p>
  <input
    bind:this={fileInput}
    type="file"
    accept=".whl,.tar.gz,.tgz,application/gzip,application/x-gzip,application/x-tar"
    multiple
    disabled={state.status !== "ready"}
    onchange={(e) => {
      const input = e.target as HTMLInputElement;
      if (input.files) handleFiles(input.files);
    }}
  />
</div>

<div class="example-row">
  <span class="example-label">Try:</span>
  {#each EXAMPLES as ex}
    <button
      class="pill"
      disabled={state.status !== "ready" || exampleLoading !== null}
      onclick={(e) => { e.stopPropagation(); installExample(ex); }}
    >
      {#if exampleLoading === ex.filename}
        <span class="pill-spinner"></span>
      {/if}
      {ex.label}
      <span class="pill-meta">{ex.meta}</span>
    </button>
  {/each}
</div>

{#if packages.length > 0}
  <h2 class="pkg-title">Packages</h2>
  {#each packages as pkg}
    <div class="pkg-row">
      <strong>{pkg.name}</strong>
      {pkg.version}
      <span class="pkg-size">{(pkg.size / 1024).toFixed(1)} KB</span>
      {#if pkg.status === "installed"}
        <span class="pkg-status installed">
          Installed ({pkg.fileCount} files{pkg.nativeExtCount ? `, ${pkg.nativeExtCount} native ext` : ""})
        </span>
      {:else if pkg.status === "installing"}
        <span class="pkg-status installing">Installing...</span>
      {:else if pkg.status === "linking"}
        <span class="pkg-status linking">Linking native extensions...</span>
      {:else if pkg.status === "error"}
        <span class="pkg-status error" title={pkg.error}>Error</span>
      {:else}
        <span class="pkg-status pending">Pending</span>
      {/if}
    </div>
  {/each}
{/if}

<CodeEditor bind:value={code} onrun={run} placeholder="Try importing an installed package..." />

<div class="btn-row">
  <button
    class="btn-primary"
    disabled={state.status !== "ready"}
    onclick={run}
  >
    Run (Ctrl+Enter)
  </button>
</div>

{#if output != null}
  <OutputBox {output} {isError} {elapsed} />
{/if}

<p class="pkg-hint">
  Download packages from <a href="https://pypi.org">PyPI</a>. Pure Python
  (<code>py3-none-any.whl</code>) and native WASI extension
  (<code>wasm32-wasi.whl</code> / <code>.tar.gz</code>) packages are supported.
</p>

<style>
  .hint {
    color: #666;
    margin-top: 0;
  }
  .status {
    padding: 12px;
    margin-bottom: 16px;
    border-radius: 6px;
    font-size: 14px;
  }
  .status.success {
    background: #d4edda;
    color: #155724;
    border: 1px solid #c3e6cb;
  }
  .status.error {
    background: #f8d7da;
    color: #721c24;
    border: 1px solid #f5c6cb;
  }
  .link-progress {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px;
    margin-bottom: 16px;
    border-radius: 6px;
    background: #e8f4fd;
    color: #0c5460;
    border: 1px solid #bee5eb;
    font-size: 14px;
  }
  .spinner {
    width: 16px;
    height: 16px;
    border: 2px solid #0c5460;
    border-top-color: transparent;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to { transform: rotate(360deg); }
  }
  .upload-zone {
    border: 2px dashed #ccc;
    border-radius: 8px;
    padding: 40px 20px;
    text-align: center;
    color: #737373;
    margin-bottom: 16px;
    transition: all 0.2s;
    cursor: pointer;
  }
  .upload-zone:hover,
  .upload-zone.dragover {
    border-color: #007bff;
    color: #007bff;
    background: #f0f7ff;
  }
  .upload-zone input[type="file"] {
    display: none;
  }
  .upload-zone p {
    margin: 0 0 8px;
  }
  .upload-zone .small {
    font-size: 13px;
    margin: 0;
  }
  .pkg-title {
    font-size: 15px;
    margin-bottom: 8px;
  }
  .pkg-row {
    padding: 8px 12px;
    background: #f1f3f5;
    border-radius: 6px;
    margin-bottom: 6px;
    font-size: 14px;
  }
  .pkg-size {
    color: #737373;
    margin-left: 8px;
  }
  .pkg-status {
    float: right;
  }
  .pkg-status.installed {
    color: #28a745;
  }
  .pkg-status.installing {
    color: #007bff;
  }
  .pkg-status.linking {
    color: #6f42c1;
  }
  .pkg-status.error {
    color: #dc3545;
  }
  .pkg-status.pending {
    color: #737373;
  }
  .pkg-hint {
    font-size: 13px;
    color: #737373;
    margin-top: 16px;
  }
  .pkg-hint a {
    color: #007bff;
  }
  .example-row {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: -8px;
    margin-bottom: 4px;
  }
  .example-label {
    font-size: 13px;
    color: #737373;
  }
  .pill-meta {
    font-size: 11px;
    color: #868e96;
    margin-left: 4px;
  }
  .pill-spinner {
    display: inline-block;
    width: 12px;
    height: 12px;
    border: 2px solid #adb5bd;
    border-top-color: transparent;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
    vertical-align: middle;
    margin-right: 4px;
  }
</style>
