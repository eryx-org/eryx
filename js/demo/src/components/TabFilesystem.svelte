<script lang="ts">
  import { getSandboxState, runCode } from "../lib/sandbox.svelte";
  import { FS_EXAMPLES } from "../lib/examples";
  import CodeEditor from "./CodeEditor.svelte";
  import ExamplePills from "./ExamplePills.svelte";
  import OutputBox from "./OutputBox.svelte";

  let state = $derived(getSandboxState());

  let code = $state(FS_EXAMPLES.list.code);

  let output: string | null = $state(null);
  let isError = $state(false);
  let elapsed: number | null = $state(null);

  // File tree state
  interface FsEntry {
    path: string;
    name: string;
    dir: string;
    type: "file" | "dir";
    size: number;
  }

  let entries: FsEntry[] = $state([]);
  let treeLoading = $state(true);
  let treeError: string | null = $state(null);

  // Editor state
  let editorOpen = $state(false);
  let editorPath = $state("");
  let editorContent = $state("");

  // Mount state
  let mountInfo: string | null = $state(null);
  let statusMessage: string | null = $state(null);
  let statusType: "success" | "error" | "info" = $state("success");

  const hasFSAccess = typeof window.showDirectoryPicker === "function";

  function showStatus(msg: string, type: "success" | "error" | "info" = "success") {
    statusMessage = msg;
    statusType = type;
    if (type !== "info") setTimeout(() => (statusMessage = null), 3000);
  }

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
    await refreshFileTree();
  }

  async function refreshFileTree() {
    const scanCode = `
import os, json
result = []
for root, dirs, files in os.walk('/data'):
    rel = root[len('/data'):] or '/'
    for d in sorted(dirs):
        result.append({'path': os.path.join(root, d), 'name': d, 'dir': rel, 'type': 'dir', 'size': 0})
    for f in sorted(files):
        full = os.path.join(root, f)
        try:
            size = os.path.getsize(full)
        except:
            size = 0
        result.append({'path': full, 'name': f, 'dir': rel, 'type': 'file', 'size': size})
print(json.dumps(result))
`;
    try {
      const result = await runCode(scanCode);
      if (result.ok && result.result.stdout) {
        entries = JSON.parse(result.result.stdout.trim());
        treeError = null;
      } else {
        treeError = "Could not read /data";
      }
    } catch (e) {
      treeError = "Could not read /data: " + (e as Error).message;
    }
    treeLoading = false;
  }

  // Initial tree load once sandbox is ready
  $effect(() => {
    if (state.status === "ready") {
      refreshFileTree();
    }
  });

  function formatSize(bytes: number): string {
    if (bytes < 1024) return bytes + " B";
    if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + " KB";
    return (bytes / (1024 * 1024)).toFixed(1) + " MB";
  }

  function entryDepth(entry: FsEntry): number {
    return entry.dir === "/"
      ? 0
      : entry.dir.split("/").filter(Boolean).length;
  }

  async function openEditor(filePath: string) {
    const readCode = `
import base64
with open(${JSON.stringify(filePath)}, 'rb') as f:
    data = f.read()
print(base64.b64encode(data).decode())
`;
    const result = await runCode(readCode);
    if (result.ok && result.result.stdout) {
      const b64 = result.result.stdout.trim();
      const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
      editorContent = new TextDecoder().decode(bytes);
      editorPath = filePath;
      editorOpen = true;
    } else {
      showStatus("Failed to read file", "error");
    }
  }

  async function saveFile() {
    const b64 = btoa(
      Array.from(new TextEncoder().encode(editorContent), (b) =>
        String.fromCharCode(b),
      ).join(""),
    );
    const writeCode = `
import base64
data = base64.b64decode(${JSON.stringify(b64)})
with open(${JSON.stringify(editorPath)}, 'wb') as f:
    f.write(data)
print(f"Saved {len(data)} bytes to ${editorPath}")
`;
    const result = await runCode(writeCode);
    if (result.ok) {
      showStatus(`Saved ${editorPath}`);
      await refreshFileTree();
    } else {
      showStatus("Save failed: " + result.error, "error");
    }
  }

  async function deleteEntry(entry: FsEntry) {
    const what = entry.type === "dir" ? "directory" : "file";
    if (!confirm(`Delete ${what} "${entry.name}"?`)) return;
    const delCode =
      entry.type === "dir"
        ? `import shutil; shutil.rmtree(${JSON.stringify(entry.path)})`
        : `import os; os.remove(${JSON.stringify(entry.path)})`;
    const result = await runCode(delCode);
    if (!result.ok) {
      showStatus("Delete failed: " + result.error, "error");
    }
    await refreshFileTree();
  }

  async function newFile() {
    const name = prompt("File name:", "new_file.txt");
    if (!name) return;
    const filePath = "/data/" + name;
    const createCode = `
import os
os.makedirs(os.path.dirname(${JSON.stringify(filePath)}), exist_ok=True)
with open(${JSON.stringify(filePath)}, 'w') as f:
    f.write('')
`;
    await runCode(createCode);
    await refreshFileTree();
    openEditor(filePath);
  }

  async function newFolder() {
    const name = prompt("Folder name:", "new_folder");
    if (!name) return;
    const dirPath = "/data/" + name;
    await runCode(
      `import os; os.makedirs(${JSON.stringify(dirPath)}, exist_ok=True)`,
    );
    await refreshFileTree();
  }

  async function mountDirectory() {
    if (!hasFSAccess) return;
    try {
      const dirHandle = await (window as any).showDirectoryPicker({
        mode: "read",
      });
      showStatus(`Importing "${dirHandle.name}"...`, "info");
      await syncHostToSandbox(dirHandle, "/data");
      mountInfo = `Imported: ${dirHandle.name} \u2192 /data (read-only)`;
      await refreshFileTree();
      showStatus(`Imported "${dirHandle.name}" \u2192 /data`);
    } catch (e: any) {
      if (e.name !== "AbortError") {
        showStatus("Mount failed: " + e.message, "error");
      }
    }
  }

  async function syncHostToSandbox(dirHandle: any, basePath: string) {
    for await (const [name, handle] of dirHandle) {
      const guestPath = basePath + "/" + name;
      if (handle.kind === "directory") {
        await runCode(
          `import os; os.makedirs(${JSON.stringify(guestPath)}, exist_ok=True)`,
        );
        await syncHostToSandbox(handle, guestPath);
      } else {
        const file = await handle.getFile();
        const buf = await file.arrayBuffer();
        const bytes = new Uint8Array(buf);
        if (bytes.length > 1024 * 1024) continue; // skip >1MB
        const b64 = btoa(
          Array.from(bytes, (b) => String.fromCharCode(b)).join(""),
        );
        await runCode(`
import base64, os
os.makedirs(os.path.dirname(${JSON.stringify(guestPath)}), exist_ok=True)
data = base64.b64decode(${JSON.stringify(b64)})
with open(${JSON.stringify(guestPath)}, 'wb') as f:
    f.write(data)
`);
      }
    }
  }
</script>

<p class="hint">
  Browse the sandbox's virtual filesystem. Import files from a local directory
  to work with them in Python.
</p>

{#if !hasFSAccess}
  <div class="info-banner">
    <strong>File System Access API not available.</strong> Use Chrome/Edge 86+
    over HTTPS to mount local directories. You can still browse and edit files in
    the sandbox VFS.
  </div>
{/if}

{#if statusMessage}
  <div
    class="status"
    class:success={statusType === "success"}
    class:error={statusType === "error"}
    class:info={statusType === "info"}
  >
    {statusMessage}
  </div>
{/if}

<ExamplePills examples={FS_EXAMPLES} onselect={(c) => (code = c)} />

<CodeEditor bind:value={code} onrun={run} />

<div class="btn-row">
  <button
    class="btn-primary"
    disabled={state.status !== "ready"}
    onclick={run}
  >
    Run (Ctrl+Enter)
  </button>
  {#if hasFSAccess}
    <button
      class="btn-success btn-sm"
      disabled={state.status !== "ready"}
      onclick={mountDirectory}
    >
      Mount Directory
    </button>
  {/if}
  <button
    class="btn-secondary btn-sm"
    disabled={state.status !== "ready"}
    onclick={refreshFileTree}
  >
    Refresh Tree
  </button>
</div>

{#if output != null}
  <OutputBox {output} {isError} {elapsed} />
{/if}

<div class="fs-explorer">
  <div class="explorer-header">
    <h3>File Explorer &mdash; <code>/data</code></h3>
    <div class="explorer-actions">
      <button
        class="btn-primary btn-sm"
        disabled={state.status !== "ready"}
        onclick={newFile}
      >
        New File
      </button>
      <button
        class="btn-secondary btn-sm"
        disabled={state.status !== "ready"}
        onclick={newFolder}
      >
        New Folder
      </button>
    </div>
  </div>
  {#if mountInfo}
    <div class="mount-info">{mountInfo}</div>
  {/if}
  <div class="fs-tree">
    {#if treeLoading}
      <span class="tree-placeholder">Loading sandbox to browse files...</span>
    {:else if treeError}
      <div class="fs-empty">{treeError}</div>
    {:else if entries.length === 0}
      <div class="fs-empty">
        Empty &mdash; create files or mount a directory
      </div>
    {:else}
      {#each entries as entry}
        <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
        <div
          class="fs-entry"
          style="padding-left: {12 + entryDepth(entry) * 16}px"
          role={entry.type === "file" ? "button" : undefined}
          tabindex={entry.type === "file" ? 0 : -1}
          onclick={() => entry.type === "file" && openEditor(entry.path)}
          onkeydown={(e) =>
            e.key === "Enter" &&
            entry.type === "file" &&
            openEditor(entry.path)}
        >
          <span class="icon"
            >{entry.type === "dir" ? "\u{1F4C1}" : "\u{1F4C4}"}</span
          >
          <span class="name"
            >{entry.name}{entry.type === "dir" ? "/" : ""}</span
          >
          <span class="meta"
            >{entry.type === "file" ? formatSize(entry.size) : ""}</span
          >
          <span class="actions-inline">
            {#if entry.type === "file"}
              <button
                class="act-btn"
                onclick={(e: MouseEvent) => { e.stopPropagation(); openEditor(entry.path); }}
                >Edit</button
              >
            {/if}
            <button
              class="act-btn del"
              onclick={(e: MouseEvent) => { e.stopPropagation(); deleteEntry(entry); }}
              >Delete</button
            >
          </span>
        </div>
      {/each}
    {/if}
  </div>
</div>

{#if editorOpen}
  <div class="editor-container">
    <div class="editor-header">
      <h3>Editing: <code>{editorPath}</code></h3>
      <div class="editor-actions">
        <button class="btn-primary btn-sm" onclick={saveFile}>Save</button>
        <button
          class="btn-secondary btn-sm"
          onclick={() => (editorOpen = false)}>Close</button
        >
      </div>
    </div>
    <textarea bind:value={editorContent} class="editor-textarea"></textarea>
  </div>
{/if}

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
  .status.info {
    background: #d1ecf1;
    color: #0c5460;
    border: 1px solid #bee5eb;
  }

  .fs-explorer {
    margin-top: 16px;
  }
  .explorer-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 8px;
  }
  .explorer-header h3 {
    font-size: 15px;
    margin: 0;
  }
  .explorer-actions {
    display: flex;
    gap: 6px;
  }
  .mount-info {
    font-size: 13px;
    color: #666;
    margin-bottom: 8px;
  }

  .fs-tree {
    background: #f8f9fa;
    border: 1px solid #dee2e6;
    border-radius: 6px;
    padding: 8px 0;
    font-size: 13px;
    min-height: 60px;
    max-height: 350px;
    overflow-y: auto;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  }
  .tree-placeholder {
    color: #888;
    font-size: 13px;
    padding: 8px 12px;
    display: block;
  }
  .fs-empty {
    padding: 16px;
    text-align: center;
    color: #888;
    font-style: italic;
  }

  .fs-entry {
    padding: 4px 12px;
    cursor: pointer;
    display: flex;
    align-items: center;
    gap: 6px;
    transition: background 0.1s;
  }
  .fs-entry:hover {
    background: #e9ecef;
  }
  .fs-entry .icon {
    width: 16px;
    text-align: center;
    flex-shrink: 0;
  }
  .fs-entry .name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .fs-entry .meta {
    color: #888;
    font-size: 12px;
    flex-shrink: 0;
  }
  .fs-entry .actions-inline {
    display: none;
    gap: 4px;
    flex-shrink: 0;
  }
  .fs-entry:hover .actions-inline {
    display: flex;
  }

  .act-btn {
    padding: 1px 6px;
    font-size: 11px;
    border-radius: 3px;
    border: 1px solid #ccc;
    background: white;
    cursor: pointer;
    color: #495057;
  }
  .act-btn:hover {
    background: #e9ecef;
  }
  .act-btn.del:hover {
    background: #f8d7da;
    color: #721c24;
    border-color: #f5c6cb;
  }

  .editor-container {
    margin-top: 16px;
  }
  .editor-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 8px;
  }
  .editor-header h3 {
    font-size: 15px;
    margin: 0;
  }
  .editor-actions {
    display: flex;
    gap: 6px;
  }
  .editor-textarea {
    min-height: 200px;
  }
</style>
