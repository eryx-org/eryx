<script lang="ts">
  import { getSandboxState } from "../lib/sandbox.svelte";

  let state = $derived(getSandboxState());
</script>

{#if state.status === "loading"}
  <div class="status info loading">
    Loading Python sandbox (~45MB)&hellip; this may take a moment.
  </div>
{:else if state.status === "relinking"}
  <div class="status info loading">
    Linking native extensions: {state.progress}
  </div>
{:else if state.status === "error"}
  <div class="status error">{state.message}</div>
{:else}
  <div class="status success">
    Sandbox loaded in {state.loadTime}s. Ready to run Python 3.14!
  </div>
{/if}

<style>
  .status {
    padding: 12px;
    margin-bottom: 20px;
    border-radius: 6px;
    font-size: 14px;
  }
  .info {
    background: var(--info-bg);
    color: var(--info);
    border: 1px solid var(--info-border);
  }
  .error {
    background: var(--danger-bg);
    color: var(--danger);
    border: 1px solid var(--danger-border);
  }
  .success {
    background: var(--success-bg);
    color: var(--success);
    border: 1px solid var(--success-border);
  }
</style>
