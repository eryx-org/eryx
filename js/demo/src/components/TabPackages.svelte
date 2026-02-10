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

  async function handleFiles(files: FileList) {
    for (const f of files) {
      if (!f.name.endsWith(".whl")) continue;
      const parts = f.name.replace(".whl", "").split("-");
      packages.push({
        name: parts[0],
        version: parts[1] || "?",
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
        await installWheel(pkg);
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

  async function installWheel(pkg: Package) {
    const arrayBuffer = await pkg.file.arrayBuffer();
    const entries = await parseZip(new Uint8Array(arrayBuffer));

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
        // Skip .so files (already linked into the component)
        if (path.endsWith(".so")) continue;

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
  Upload Python wheels to make packages available for import. Pure-Python
  wheels are extracted into a virtual filesystem. Wheels with native WASI
  extensions trigger an in-browser linking pipeline.
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
  <p><strong>Drop .whl files here</strong> or click to browse</p>
  <p class="small">Pure Python and native WASI extension wheels are supported</p>
  <input
    bind:this={fileInput}
    type="file"
    accept=".whl"
    multiple
    disabled={state.status !== "ready"}
    onchange={(e) => {
      const input = e.target as HTMLInputElement;
      if (input.files) handleFiles(input.files);
    }}
  />
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
  Download wheels from <a href="https://pypi.org">PyPI</a>. Pure Python
  (<code>py3-none-any.whl</code>) and native WASI extension
  (<code>wasm32-wasi.whl</code>) wheels are supported.
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
</style>
