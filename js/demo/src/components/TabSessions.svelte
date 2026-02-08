<script lang="ts">
  import { getSandboxState, runCode } from "../lib/sandbox.svelte";
  import { SESSION_EXAMPLES } from "../lib/examples";
  import CodeEditor from "./CodeEditor.svelte";
  import ExamplePills from "./ExamplePills.svelte";
  import OutputBox from "./OutputBox.svelte";

  let state = $derived(getSandboxState());

  let code = $state(
    `x = 42
y = "hello"
items = [1, 2, 3]
print(f"x={x}, y={y}, items={items}")`,
  );
  let output: string | null = $state(null);
  let isError = $state(false);
  let elapsed: number | null = $state(null);

  interface Snapshot {
    data: Uint8Array;
    time: Date;
  }

  let snapshots: Map<string, Snapshot> = $state(new Map());
  let statusMessage: string | null = $state(null);
  let statusType: "success" | "error" = $state("success");

  function showStatus(msg: string, type: "success" | "error" = "success") {
    statusMessage = msg;
    statusType = type;
    setTimeout(() => (statusMessage = null), 3000);
  }

  async function run() {
    const trimmed = code.trim();
    if (!trimmed) return;
    const result = await runCode(trimmed);
    if (result.ok) {
      let text = "";
      if (result.result.stdout) text += result.result.stdout;
      if (result.result.stderr) {
        if (text) text += "\n";
        text += "[stderr] " + result.result.stderr;
      }
      output = text || "(no output)";
      isError = false;
    } else {
      output = "Error: " + result.error;
      isError = true;
    }
    elapsed = result.elapsed;
  }

  async function takeSnapshot() {
    if (state.status !== "ready") return;
    const name = prompt("Snapshot name:", `snapshot-${snapshots.size + 1}`);
    if (!name) return;
    try {
      const raw = await state.sandbox.snapshotState();
      const data = new Uint8Array(raw);
      snapshots.set(name, { data, time: new Date() });
      snapshots = new Map(snapshots); // trigger reactivity
      showStatus(
        `Snapshot "${name}" saved (${(data.byteLength / 1024).toFixed(1)} KB)`,
      );
    } catch (e) {
      showStatus("Snapshot failed: " + (e as Error).message, "error");
    }
  }

  async function restoreSnapshot(name: string) {
    if (state.status !== "ready") return;
    const snap = snapshots.get(name);
    if (!snap) return;
    await state.sandbox.restoreState(snap.data);
    showStatus(`Restored "${name}"`);
  }

  function deleteSnapshot(name: string) {
    snapshots.delete(name);
    snapshots = new Map(snapshots);
  }

  async function clearState() {
    if (state.status !== "ready") return;
    await state.sandbox.clearState();
    showStatus("State cleared");
  }

  function formatSize(bytes: number): string {
    return (bytes / 1024).toFixed(1);
  }
</script>

<p class="hint">
  Variables persist across runs. Use snapshots to save and restore interpreter
  state.
</p>

{#if statusMessage}
  <div class="status" class:success={statusType === "success"} class:error={statusType === "error"}>
    {statusMessage}
  </div>
{/if}

<ExamplePills examples={SESSION_EXAMPLES} onselect={(c) => (code = c)} />

<CodeEditor bind:value={code} onrun={run} />

<div class="btn-row">
  <button
    class="btn-primary"
    disabled={state.status !== "ready"}
    onclick={run}
  >
    Run
  </button>
  <button
    class="btn-success btn-sm"
    disabled={state.status !== "ready"}
    onclick={takeSnapshot}
  >
    Snapshot State
  </button>
  <button
    class="btn-warning btn-sm"
    disabled={state.status !== "ready"}
    onclick={clearState}
  >
    Clear State
  </button>
</div>

{#if output != null}
  <OutputBox {output} {isError} {elapsed} />
{/if}

{#if snapshots.size > 0}
  <div class="snapshots">
    <h2>Saved Snapshots</h2>
    <div class="snapshots-list">
      {#each [...snapshots] as [name, snap]}
        <div class="snapshot-card">
          <div class="name">{name}</div>
          <div class="meta">
            {snap.time.toLocaleTimeString()} &middot; {formatSize(
              snap.data.byteLength,
            )} KB
          </div>
          <div class="actions">
            <button
              class="btn-primary btn-sm"
              onclick={() => restoreSnapshot(name)}>Restore</button
            >
            <button
              class="btn-danger btn-sm"
              onclick={() => deleteSnapshot(name)}>Delete</button
            >
          </div>
        </div>
      {/each}
    </div>
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
  .snapshots h2 {
    font-size: 15px;
    margin-bottom: 8px;
  }
  .snapshots-list {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    margin-top: 16px;
  }
  .snapshot-card {
    background: #f8f9fa;
    border: 1px solid #dee2e6;
    border-radius: 8px;
    padding: 12px 16px;
    min-width: 180px;
  }
  .snapshot-card .name {
    font-weight: 600;
    font-size: 14px;
  }
  .snapshot-card .meta {
    font-size: 12px;
    color: #737373;
    margin: 4px 0 8px;
  }
  .snapshot-card .actions {
    display: flex;
    gap: 6px;
  }
</style>
