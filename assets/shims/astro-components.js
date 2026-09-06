import { createComponent, render, escapeHTML, unescapeHTML, addAttribute } from "/__sl/astro.js";
export { Image, Picture, Font } from "/__sl/shim/astro-assets.js";
export { ClientRouter, ViewTransitions } from "/__sl/shim/astro-transitions.js";
export const Code = createComponent((result, props) => {
  const { code = "", lang = "", class: cls = "", ...rest } = props;
  return render`<pre${addAttribute(["astro-code", cls].filter(Boolean).join(" "), "class")}${addAttribute(lang, "data-language")}><code>${escapeHTML(String(code))}</code></pre>`;
}, "astro/components/Code");
export const Prism = Code;
export const Debug = createComponent((result, props) => render`<pre class="astro-debug">${escapeHTML(JSON.stringify(props, null, 2))}</pre>`, "astro/components/Debug");
export const Welcome = createComponent(() => render`<p>Welcome</p>`, "astro/components/Welcome");
