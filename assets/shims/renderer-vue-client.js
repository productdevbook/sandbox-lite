import { Suspense, createApp, createSSRApp, h } from "vue";
import StaticHtml from "/__sl/shim/vue-static-html.js";

const apps = new WeakMap();

const isAsync = (fn) => fn?.constructor?.name === "AsyncFunction";

export default (element) => async (Component, props, slotted, { client }) => {
  if (!element.hasAttribute("ssr")) return;
  const slots = {};
  for (const [key, value] of Object.entries(slotted || {})) {
    slots[key] = () => h(StaticHtml, { value, name: key === "default" ? undefined : key });
  }
  const existing = apps.get(element);
  if (existing) {
    existing.props = props;
    existing.slots = slots;
    existing.component.$forceUpdate();
    return;
  }
  const instance = { props, slots };
  const hydrating = client !== "only";
  const app = (hydrating ? createSSRApp : createApp)({
    name: Component.name ? `${Component.name} Host` : undefined,
    render() {
      instance.component = this;
      const content = h(Component, instance.props, instance.slots);
      return isAsync(Component.setup) ? h(Suspense, null, content) : content;
    },
  });
  app.config.idPrefix = element.getAttribute("prefix") ?? undefined;
  app.mount(element, hydrating);
  apps.set(element, instance);
  element.addEventListener("astro:unmount", () => app.unmount(), { once: true });
};
