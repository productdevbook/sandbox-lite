#!/usr/bin/env bash
# Bundles Astro's own server runtime + container API into one browser ESM file.
# Only needed when bumping the Astro version; the output is committed under assets/.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
ASTRO_VERSION="${ASTRO_VERSION:-7.3.1}"
ESBUILD_VERSION="${ESBUILD_VERSION:-0.28.2}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$work"
npm init -y >/dev/null
npm i --no-audit --no-fund --silent "astro@$ASTRO_VERSION" "esbuild@$ESBUILD_VERSION"
cat > entry.js <<'JS'
export * from "astro/compiler-runtime";
export { experimental_AstroContainer } from "astro/container";
JS
banner='import.meta.env = globalThis.__sl_env || { DEV: true, PROD: false, MODE: "development", SSR: true, BASE_URL: "/" };
var process = globalThis.process || { env: { NODE_ENV: "development" }, versions: {} };'
npx esbuild entry.js --bundle --format=esm --platform=browser --target=es2022 \
  --minify-syntax --minify-whitespace --legal-comments=none \
  --banner:js="$banner" --outfile=astro.js
cp astro.js "$root/assets/astro.js"
if [ -f node_modules/astro/components/viewtransitions.css ]; then
  cp node_modules/astro/components/viewtransitions.css "$root/assets/viewtransitions.css"
fi
echo "$ASTRO_VERSION" > "$root/assets/astro.version"
ls -la "$root/assets/astro.js"
