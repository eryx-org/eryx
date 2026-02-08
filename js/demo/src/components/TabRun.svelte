<script lang="ts">
  import { getSandboxState, runCode } from "../lib/sandbox.svelte";
  import { RUN_EXAMPLES } from "../lib/examples";
  import CodeEditor from "./CodeEditor.svelte";
  import ExamplePills from "./ExamplePills.svelte";
  import OutputBox from "./OutputBox.svelte";

  let state = $derived(getSandboxState());
  let ready = $derived(state.status === "ready");

  let code = $state(
    `print("Hello from eryx!")
result = sum(range(10))
print(f"Sum of 0..9 = {result}")`,
  );
  let running = $state(false);
  let output: string | null = $state(null);
  let isError = $state(false);
  let elapsed: number | null = $state(null);

  // Restore code from URL hash
  if (window.location.hash) {
    try {
      code = decodeURIComponent(window.location.hash.slice(1));
    } catch {}
  }

  // Save code to URL hash on change
  $effect(() => {
    history.replaceState(null, "", "#" + encodeURIComponent(code));
  });

  async function run() {
    const trimmed = code.trim();
    if (!trimmed || running) return;

    running = true;
    output = "";
    const result = await runCode(trimmed);
    running = false;

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
</script>

<p class="share-hint">Code is saved in the URL for sharing.</p>

<ExamplePills
  examples={RUN_EXAMPLES}
  onselect={(c) => (code = c)}
/>

<CodeEditor bind:value={code} onrun={run} />

<div class="btn-row">
  <button class="btn-primary" disabled={!ready || running} onclick={run}>
    {running ? "Running..." : "Run (Ctrl+Enter)"}
  </button>
  <button
    class="btn-secondary"
    onclick={() => {
      output = null;
      elapsed = null;
    }}
  >
    Clear Output
  </button>
</div>

{#if output != null}
  <OutputBox {output} {isError} {elapsed} showCopy />
{/if}

<style>
  .share-hint {
    color: #666;
    margin-top: 0;
    font-size: 13px;
  }
</style>
