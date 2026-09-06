<script setup>
import { ref } from "vue";

const props = defineProps({
  label: { type: String, required: true },
  start: { type: Number, default: 0 },
});

const n = ref(props.start);
</script>

<template>
  <div class="counter" :data-count="n">
    <span>{{ label }}</span>
    <button type="button" aria-label="decrement" @click="n--">−</button>
    <strong>{{ n }}</strong>
    <button type="button" aria-label="increment" @click="n++">+</button>
  </div>
</template>

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
  background: #41b883;
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
