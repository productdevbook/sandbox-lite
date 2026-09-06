import { render } from "svelte/server";

// svelte/server's own createRawSnippet, which the browser build of `svelte` does not export.
const rawSnippet = (html) => (target) => (typeof target.push === "function" ? target.push(html) : target.out.push(html));

const serverComponent = (Component) => (typeof Component === "function" ? Component.__sl_svelte_server || Component : undefined);

async function check(Component) {
  const server = serverComponent(Component);
  if (!server) return false;
  if (Component.__sl_svelte_server) return true;
  const source = server.toString();
  return source.includes("$$payload") || source.includes("$$renderer");
}

async function renderToStaticMarkup(Component, props, slotted, metadata) {
  const hydrate = metadata?.astroStaticSlot ? !!metadata.hydrate : true;
  const tag = hydrate ? "astro-slot" : "astro-static-slot";
  const renderProps = { ...props };
  let $$slots;
  for (const [key, value] of Object.entries(slotted || {})) {
    $$slots ??= {};
    const html = key === "default" ? `<${tag}>${value}</${tag}>` : `<${tag} name="${key}">${value}</${tag}>`;
    const snippet = rawSnippet(html);
    $$slots[key] = key === "default" ? true : snippet;
    renderProps[key === "default" ? "children" : key] = snippet;
  }
  if ($$slots) renderProps.$$slots = $$slots;
  const result = await render(serverComponent(Component), { props: renderProps });
  return { html: result.body.replace(/\s+class=""/g, "") };
}

export default { name: "@astrojs/svelte", check, renderToStaticMarkup, supportsAstroStaticSlot: true };
