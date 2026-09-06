import { defineComponent, h } from "vue";

export default defineComponent({
  props: { value: String, name: String, hydrate: { type: Boolean, default: true } },
  setup({ name, value, hydrate }) {
    if (!value) return () => null;
    const tag = hydrate ? "astro-slot" : "astro-static-slot";
    return () => h(tag, { name, innerHTML: value });
  },
});
