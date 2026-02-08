<script lang="ts">
  import { getSandboxState } from "../lib/sandbox.svelte";

  let state = $derived(getSandboxState());
</script>

{#if state.status === "loading"}
  <div class="status info loading">
    Loading Python sandbox (~45MB)&hellip; this may take a moment.
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
    background: #d1ecf1;
    color: #0c5460;
    border: 1px solid #bee5eb;
  }
  .error {
    background: #f8d7da;
    color: #721c24;
    border: 1px solid #f5c6cb;
  }
  .success {
    background: #d4edda;
    color: #155724;
    border: 1px solid #c3e6cb;
  }
</style>
