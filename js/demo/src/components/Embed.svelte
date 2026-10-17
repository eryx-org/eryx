<script lang="ts">
  import { onMount } from "svelte";
  import { getSandboxState, initSandbox, runCode } from "../lib/sandbox.svelte";
  import CodeEditor from "./CodeEditor.svelte";
  import OutputBox from "./OutputBox.svelte";

  // Compact view for embedding on eryx.run (?embed). It provides the
  // `get_user` host callback used by the landing page's example.
  let code = $state(`# get_user is a host function, provided by this page
user = await get_user(id=123)
print(f"Hello {user['name']}")`);

  let state = $derived(getSandboxState());
  let ready = $derived(state.status === "ready");
  let running = $state(false);
  let output: string | null = $state(null);
  let isError = $state(false);
  let elapsed: number | null = $state(null);

  async function run() {
    const trimmed = code.trim();
    if (!trimmed || running || !ready) return;
    running = true;
    const result = await runCode(trimmed);
    running = false;
    if (result.ok) {
      const { stdout, stderr } = result.result;
      output = [stdout, stderr && "[stderr] " + stderr].filter(Boolean).join("\n") || "(no output)";
      isError = false;
    } else {
      output = "Error: " + result.error;
      isError = true;
    }
    elapsed = result.elapsed;
  }

  onMount(async () => {
    const { setCallbacks, setCallbackHandler } = await import("@bsull/eryx");
    setCallbacks([{ name: "get_user", description: "Look up a user by id" }]);
    setCallbackHandler((name, argsJson) => {
      if (name !== "get_user") throw new Error(`Unknown callback: ${name}`);
      const { id } = JSON.parse(argsJson);
      return JSON.stringify({ id, name: "Alice" });
    });
    await initSandbox();
    await run();
  });
</script>

<div class="embed">
  <CodeEditor bind:value={code} onrun={run} />
  <div class="btn-row">
    <button class="btn-primary" disabled={!ready || running} onclick={run}>
      {running ? "Running..." : "Run (Ctrl+Enter)"}
    </button>
    <a class="full-demo" href="/" target="_blank" rel="noopener">Open full demo ↗</a>
  </div>
  {#if state.status === "ready" && output != null}
    <OutputBox {output} {isError} {elapsed} />
  {:else if state.status === "error"}
    <div class="output-box error-output">{state.message}</div>
  {:else}
    <div class="output-box loading">
      {state.status === "ready" ? "Running..." : "Loading Python sandbox (~45MB)…"}
    </div>
  {/if}
</div>

<style>
  .embed {
    padding: 16px;
  }
  .embed :global(textarea) {
    min-height: 120px;
  }
  .btn-row {
    align-items: center;
  }
  .full-demo {
    margin-left: auto;
    font-size: 13px;
    color: var(--text-muted);
    text-decoration: none;
  }
  .full-demo:hover {
    color: var(--text);
  }
  :global(html.embed),
  :global(html.embed body) {
    background: transparent;
  }
  :global(html.embed body::before) {
    display: none;
  }
</style>
