<script lang="ts">
  interface Props {
    examples: Record<string, { label: string; code: string }>;
    onselect: (code: string) => void;
  }

  let { examples, onselect }: Props = $props();
</script>

<div class="examples">
  <h3>Examples</h3>
  <div class="pill-row">
    {#each Object.entries(examples) as [_key, ex]}
      <button class="pill" onclick={() => onselect(ex.code)}>{ex.label}</button>
    {/each}
  </div>
</div>
