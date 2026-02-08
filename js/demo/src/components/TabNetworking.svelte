<script lang="ts">
  import { getSandboxState, runCode } from "../lib/sandbox.svelte";
  import CodeEditor from "./CodeEditor.svelte";
  import OutputBox from "./OutputBox.svelte";

  let state = $derived(getSandboxState());

  let code = $state(`import socket

# eryx supports TCP and TLS via the eryx:net WIT interfaces.
# In the JS bindings, networking stubs are not yet implemented.
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
try:
    sock.connect(("example.com", 80))
    sock.sendall(b"GET / HTTP/1.0\\r\\nHost: example.com\\r\\n\\r\\n")
    data = sock.recv(512)
    print(data.decode()[:200])
except Exception as e:
    print(f"Error: {e}")
finally:
    sock.close()`);

  let output: string | null = $state(null);
  let isError = $state(false);

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
  }
</script>

<div class="info-banner">
  <strong>Coming soon.</strong> Networking is available in the Rust and Python SDKs.
  The JavaScript bindings currently return stub errors. This tab shows what the
  API looks like.
</div>

<CodeEditor bind:value={code} onrun={run} />

<div class="btn-row">
  <button
    class="btn-primary"
    disabled={state.status !== "ready"}
    onclick={run}
  >
    Run
  </button>
</div>

{#if output != null}
  <OutputBox {output} {isError} />
{/if}
