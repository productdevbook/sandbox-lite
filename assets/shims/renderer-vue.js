import { createSSRApp, h } from "vue";
import { renderToString } from "vue/server-renderer";
import StaticHtml from "/__sl/shim/vue-static-html.js";

const counters = new WeakMap();
let standalone = 0;

// Vue's useId needs a prefix that is stable per island; the client entrypoint reads it back off the element.
function nextIdPrefix(result) {
  if (!result) return `s${standalone++}`;
  const n = counters.get(result) || 0;
  counters.set(result, n + 1);
  return `s${n}`;
}

async function check(Component) {
  if (!Component || typeof Component !== "object") return false;
  return !!(Component.__sl_vue || Component.ssrRender || Component.__ssrInlineRender || Component.render || Component.setup);
}

async function renderToStaticMarkup(Component, inputProps, slotted, metadata) {
  const prefix = nextIdPrefix(this && this.result);
  const hydrate = metadata?.astroStaticSlot ? !!metadata.hydrate : true;
  const { slot: _drop, ...props } = inputProps || {};
  const slots = {};
  for (const [key, value] of Object.entries(slotted || {})) {
    slots[key] = () => h(StaticHtml, { value, name: key === "default" ? undefined : key, hydrate });
  }
  const app = createSSRApp({ render: () => h(Component, props, slots) });
  app.config.idPrefix = prefix;
  return { html: await renderToString(app), attrs: { prefix } };
}

export default { name: "@astrojs/vue", check, renderToStaticMarkup, supportsAstroStaticSlot: true };
