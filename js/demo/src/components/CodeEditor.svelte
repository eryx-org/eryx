<script lang="ts">
  interface Props {
    value: string;
    placeholder?: string;
    onrun?: () => void;
  }

  let { value = $bindable(), placeholder = "Enter Python code...", onrun }: Props = $props();

  function handleKeydown(e: KeyboardEvent) {
    const ta = e.target as HTMLTextAreaElement;
    if (e.key === "Tab") {
      e.preventDefault();
      const start = ta.selectionStart;
      ta.value =
        ta.value.substring(0, start) + "    " + ta.value.substring(ta.selectionEnd);
      ta.selectionStart = ta.selectionEnd = start + 4;
      value = ta.value;
    }
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      onrun?.();
    }
  }
</script>

<textarea
  bind:value
  {placeholder}
  onkeydown={handleKeydown}
></textarea>
