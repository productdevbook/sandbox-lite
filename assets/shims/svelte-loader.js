import { compile } from "svelte/compiler";

const SVELTE = "%SVELTE%";
const styles = (globalThis.__sl_css ||= new Map());

function rewrite(code, base) {
  return code.replace(/(\bfrom\s*|\bimport\s*)(["'])([^"'\n]+)\2/g, (whole, keyword, quote, spec) => {
    if (spec === "svelte" || spec.startsWith("svelte/")) return `${keyword}${quote}${SVELTE}${spec.slice(6)}${quote}`;
    if (spec.startsWith("./") || spec.startsWith("../")) return `${keyword}${quote}${new URL(spec, base).href}${quote}`;
    return whole;
  });
}

function load(code, base) {
  const source = `import.meta.env = globalThis.__sl_env || {};\n${rewrite(code, base)}`;
  return import(URL.createObjectURL(new Blob([source], { type: "text/javascript" })));
}

export async function compileComponent(source, path, base) {
  const name = (path.split("/").pop() || "Component").replace(/\.svelte$/, "");
  const options = { filename: path, name, css: "external", dev: false };
  const client = compile(source, { ...options, generate: "client" });
  const server = compile(source, { ...options, generate: "server" });
  if (client.css && client.css.code) styles.set(path, client.css.code);
  const [browser, ssr] = await Promise.all([load(client.js.code, base), load(server.js.code, base)]);
  // The island imports this module to hydrate, so the client build is the default export
  // and the server renderer reads the server build off it.
  browser.default.__sl_svelte_server = ssr.default;
  return browser.default;
}
