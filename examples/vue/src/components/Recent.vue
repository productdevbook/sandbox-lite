<script setup lang="ts" generic="T extends { id: number; label: string }">
interface Props {
  title: string;
  items: T[];
}

const props = defineProps<Props>();
</script>

<template>
  <section class="recent">
    <h2>{{ props.title }}</h2>
    <ul>
      <li v-for="item in props.items" :key="item.id">{{ item.label }}</li>
    </ul>
  </section>
</template>

<style>
.recent {
  margin-top: 32px;
  padding: 18px;
  border-radius: 14px;
  border: 1px dashed #26305a;
}
.recent h2 {
  margin: 0 0 6px;
  font-size: 18px;
}
.recent ul {
  margin: 0;
  padding-left: 18px;
}
</style>
