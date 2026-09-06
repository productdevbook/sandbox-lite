#!/usr/bin/env bash
# Bundles Astro's own server runtime + container API into one browser ESM file, and the
# JSX runtime + `astro:jsx` renderer (from @astrojs/mdx) into a second one that imports the first.
# Only needed when bumping the Astro version; the output is committed under assets/.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
ASTRO_VERSION="${ASTRO_VERSION:-7.3.1}"
MDX_VERSION="${MDX_VERSION:-8.0.0}"
ESBUILD_VERSION="${ESBUILD_VERSION:-0.28.2}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$work"
npm init -y >/dev/null
npm i --no-audit --no-fund --silent "astro@$ASTRO_VERSION" "@astrojs/mdx@$MDX_VERSION" "esbuild@$ESBUILD_VERSION"
cat > entry.js <<'JS'
export * from "astro/compiler-runtime";
export { experimental_AstroContainer } from "astro/container";
export { Renderer, __astro_tag_component__, chunkToString, escapeHTML, markHTMLString, renderStreaming } from "astro/runtime/server/index.js";
export { AstroError } from "astro/errors";
JS
banner='import.meta.env = globalThis.__sl_env || { DEV: true, PROD: false, MODE: "development", SSR: true, BASE_URL: "/" };
var process = globalThis.process || { env: { NODE_ENV: "development" }, versions: {} };'
npx esbuild entry.js --bundle --format=esm --platform=browser --target=es2022 \
  --minify-syntax --minify-whitespace --legal-comments=none \
  --banner:js="$banner" --outfile=astro.js
cp astro.js "$root/assets/astro.js"
cat > entry-jsx.js <<'JS'
export * from "astro/jsx-runtime";
export { default } from "@astrojs/mdx/server.js";
JS
# The runtime must stay one instance (SlotString and friends are checked with instanceof),
# so whatever the JSX modules take from it is imported from astro.js instead of bundled again.
cat > build-jsx.mjs <<'JS'
import { build } from "esbuild";
await build({
  entryPoints: ["entry-jsx.js"],
  bundle: true, format: "esm", platform: "browser", target: "es2022",
  minifySyntax: true, minifyWhitespace: true, legalComments: "none",
  outfile: "astro-jsx.js",
  plugins: [{
    name: "runtime-from-astro-js",
    setup(b) {
      b.onResolve({ filter: /(^|\/)runtime\/server\/index\.js$|^astro\/errors$/ }, () => ({ path: "/__sl/astro.js", external: true }));
    },
  }],
});
JS
node build-jsx.mjs
cp astro-jsx.js "$root/assets/astro-jsx.js"
if [ -f node_modules/astro/components/viewtransitions.css ]; then
  cp node_modules/astro/components/viewtransitions.css "$root/assets/viewtransitions.css"
fi
echo "$ASTRO_VERSION" > "$root/assets/astro.version"
ls -la "$root/assets/astro.js" "$root/assets/astro-jsx.js"
