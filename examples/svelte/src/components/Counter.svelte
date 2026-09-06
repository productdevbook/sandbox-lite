<script>
  let { label, start = 0 } = $props();
  let delta = $state(0);
  let n = $derived(start + delta);
</script>

<div class="counter" data-count={n}>
  <span>{label}</span>
  <button type="button" aria-label="decrement" onclick={() => delta--}>−</button>
  <strong>{n}</strong>
  <button type="button" aria-label="increment" onclick={() => delta++}>+</button>
</div>

<style>
  .counter {
    display: inline-flex;
    align-items: center;
    gap: 12px;
    padding: 14px 18px;
    border-radius: 14px;
    background: #141b33;
    border: 1px solid #26305a;
  }
  .counter button {
    width: 32px;
    height: 32px;
    border-radius: 50%;
    border: none;
    background: #ff3e00;
    color: white;
    font-size: 18px;
    cursor: pointer;
  }
  .counter strong {
    min-width: 2ch;
    text-align: center;
    font-size: 20px;
  }
</style>
