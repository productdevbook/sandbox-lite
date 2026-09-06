import { createRawSnippet, hydrate, mount, unmount } from "svelte";
import { proxy } from "svelte/internal/client";

const apps = new WeakMap();

export default (element) => async (Component, props, slotted, { client }) => {
  if (!element.hasAttribute("ssr")) return;
  const renderProps = { ...props };
  let $$slots;
  for (const [key, value] of Object.entries(slotted || {})) {
    $$slots ??= {};
    const html = key === "default" ? `<astro-slot>${value}</astro-slot>` : `<astro-slot name="${key}">${value}</astro-slot>`;
    const snippet = createRawSnippet(() => ({ render: () => html }));
    $$slots[key] = key === "default" ? true : snippet;
    renderProps[key === "default" ? "children" : key] = snippet;
  }
  if ($$slots) renderProps.$$slots = $$slots;
  const existing = apps.get(element);
  if (existing) return existing.setProps(renderProps);

  const shouldHydrate = client !== "only";
  if (!shouldHydrate) element.innerHTML = "";
  // What `let props = $state(props)` compiles to, so a re-render reaches the component.
  const state = proxy(renderProps);
  const component = (shouldHydrate ? hydrate : mount)(Component, { target: element, props: state });
  const app = {
    setProps(next) {
      Object.assign(state, next);
      for (const key in state) if (!(key in next)) delete state[key];
    },
    destroy: () => unmount(component),
  };
  apps.set(element, app);
  element.addEventListener("astro:unmount", () => app.destroy(), { once: true });
};
