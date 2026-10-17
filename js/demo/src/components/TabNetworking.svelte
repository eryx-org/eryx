<script lang="ts">
  import { getSandboxState, runCode } from "../lib/sandbox.svelte";
  import { NET_EXAMPLES } from "../lib/examples";
  import CodeEditor from "./CodeEditor.svelte";
  import ExamplePills from "./ExamplePills.svelte";
  import OutputBox from "./OutputBox.svelte";

  let state = $derived(getSandboxState());

  let code = $state(NET_EXAMPLES.github.code);
  let running = $state(false);
  let output: string | null = $state(null);
  let isError = $state(false);

  async function run() {
    const trimmed = code.trim();
    if (!trimmed || running) return;
    running = true;
    const result = await runCode(trimmed);
    running = false;
    if (result.ok) {
      output = result.result.stdout || result.result.stderr || "(no output)";
      isError = false;
    } else {
      output = "Error: " + result.error;
      isError = true;
    }
  }
</script>

<div class="info-banner">
  <strong>HTTP(S) from Python, in your browser.</strong> Sandboxed code uses the
  standard <code>socket</code>, <code>ssl</code> and <code>urllib</code> modules; the
  host replays each request through <code>fetch()</code>, so only HTTP works and the
  server must allow cross-origin requests. In Node.js, and in the Rust and Python
  SDKs, sandboxes get real TCP and TLS sockets.
</div>

<ExamplePills examples={NET_EXAMPLES} onselect={(c) => (code = c)} />

<CodeEditor bind:value={code} onrun={run} />

<div class="btn-row">
  <button
    class="btn-primary"
    disabled={state.status !== "ready" || running}
    onclick={run}
  >
    {running ? "Running..." : "Run"}
  </button>
</div>

{#if output != null}
  <OutputBox {output} {isError} />
{/if}
