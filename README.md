# sandbox-lite

Live preview for many Astro sites from one small daemon. No Node.js process per
tenant, no Vite dev server per tenant: a single native binary compiles files on
demand and the visitor's browser renders the page with Astro's own runtime.

```
editor / AI agent ──PUT /api/t/acme/file/src/pages/index.astro──▶ sandbox-lite (Rust)
                                                                     │
browser at http://acme.localhost:4321/ ◀── ES modules + SSE ─────────┘
        │
        └── imports astro/container (bundled once) and renders the page itself
```

## Why

Every "edit your site with AI, watch it change live" product ends up running one
`astro dev` per customer. Each of those is a Node.js process holding Vite, the
compiler, a module graph and a transform cache — a few hundred megabytes that
sit idle between keystrokes. At a thousand customers that is the whole machine.

Two facts make that unnecessary:

1. Astro's compiler (`@astrojs/compiler-rs`) is a Rust library. It links into
   one binary; compiling a component takes microseconds and no runtime.
2. Astro's server runtime and its `container` API are plain JavaScript that
   runs in any engine — including the one already open in the customer's tab.

So the daemon only does what needs a file system view: keep each tenant's edits
as an overlay on a shared base project, compile the file that was asked for,
rewrite its imports into URLs, and tell open previews when something changed.
Rendering, routing, `getStaticPaths`, slots, scoped styles — all of it runs in
the browser, using the same code Astro ships.

What works in the preview today: `.astro` pages, layouts and components;
TypeScript and JSX; scoped `<style>` blocks, plain CSS, Sass (`lang="scss"`
and `.scss` files, compiled in-process) and Tailwind v4 (via its browser
build); content collections (`getCollection`, `getEntry`, `render`); Markdown
pages with `layout:`; dynamic routes with `getStaticPaths` and `paginate`;
`import.meta.glob` (eager and lazy, `import:`/`query:` options); `import.meta.env`
and `.env` `PUBLIC_*` variables; `tsconfig` path aliases; npm packages from a
CDN pinned to the versions in `package.json`; React and Preact islands
(`client:load` and friends hydrate with the framework's own client entrypoint);
`<script>` tags, `public/` files and live reload.

## What it costs

`bench/mem.sh` starts the release binary, creates N tenants from the starter
project, edits one file in each, loads the preview module graph of each, and
reads the daemon's RSS from `/proc`.

Release build on Linux x86-64, three base projects loaded, every tenant with
one edited page and a loaded preview:

| | daemon RSS |
|---|---|
| idle | 6.3 MB |
| after 200 tenants (edit + preview each) | 12.7 MB — 32 kB per tenant |
| after 1000 tenants (edit + preview each) | 17.5 MB — 11 kB per tenant |

Creating a tenant, writing its edit and fetching its whole module graph took
~90 ms per tenant through the HTTP API. After 1000 tenants the transform cache
held 1008 entries in 568 kB (1000 unique edited pages plus the 8 shared modules
every tenant reuses), 7993 hits to 1008 misses. The binary is 13.8 MB.

For comparison, one `astro dev` process for the same starter project is a
Node.js process of several hundred MB; a thousand of them do not fit on a
machine at all.

Per-tenant state is the overlay (only edited files) plus a content-addressed
transform cache shared by every tenant: two tenants with the same `Header.astro`
share one compiled output. The cache has a byte budget (`--cache-mb`, default
64), so its share of the daemon's memory is bounded regardless of tenant count;
each overlay is capped by `--tenant-quota-mb` (default 64), and a write that
would push a tenant past it is refused with `413`.

## Install

Every [release](https://github.com/productdevbook/sandbox-lite/releases) carries
a stripped binary for Linux (x86-64 and arm64, glibc 2.35 or newer) and macOS
(Intel and Apple silicon) as `sandbox-lite-<tag>-<target>.tar.gz`, holding
`sandbox-lite`, `LICENSE` and `README.md`, plus a `SHA256SUMS` file:

```sh
tag=v0.1.0
target=x86_64-unknown-linux-gnu   # or aarch64-unknown-linux-gnu, x86_64-apple-darwin, aarch64-apple-darwin
curl -fsSL "https://github.com/productdevbook/sandbox-lite/releases/download/$tag/sandbox-lite-$tag-$target.tar.gz" | tar xzf - sandbox-lite
sudo install sandbox-lite /usr/local/bin/
sandbox-lite --bases path/to/your/astro-projects
```

The same tag is published as a multi-arch image (`linux/amd64`, `linux/arm64`)
at `ghcr.io/productdevbook/sandbox-lite:<tag>`, and `:latest` follows the newest
release; see [Docker](#docker) for what the image bundles:

```sh
docker run -p 4321:4321 -v sandbox-data:/data ghcr.io/productdevbook/sandbox-lite:latest
```

Or build from source with a current stable Rust toolchain. The browser runtime
is embedded in the binary, so nothing else is needed at run time:

```sh
cargo install --locked --git https://github.com/productdevbook/sandbox-lite   # --tag v0.1.0 pins a release
```

## Try it

```sh
cargo build --release
./target/release/sandbox-lite            # loads ./examples/* as base projects
```

Open <http://localhost:4321/>, create a tenant `acme` from the `starter` base,
and the preview appears at <http://acme.localhost:4321/> (`*.localhost` resolves
to 127.0.0.1 in Chrome and Firefox without any setup). Edit a file on the left;
the preview reloads.

With `ANTHROPIC_API_KEY` set, the Chat tab talks to Claude with `read_file`,
`write_file`, `delete_file`, `list_files` and `check_site` tools scoped to that
tenant. Every write shows up in the preview as it happens.

### Flags

```
--listen ADDR        bind address (default 127.0.0.1:4321)
--domain NAME        tenants are served at http://<id>.NAME:PORT/ (default localhost)
--bases DIR          directory whose sub-directories are base projects
--base NAME=PATH     add one base project (repeatable)
--data-dir DIR       where tenant edits are persisted (default ./data)
--no-persist         keep edits in memory only
--cdn URL            where bare npm imports resolve in the browser (default https://esm.sh)
--cache-mb N         transform cache budget (default 64)
--model NAME         Claude model for the chat endpoint (default claude-fable-5-1)
--api-token TOKEN    require `Authorization: Bearer TOKEN` (or `?token=`) on /api/*
--preview-secret S   tenant hosts require a per-tenant token derived from S
--tenant-quota-mb N  edited files a tenant may hold, in MiB (default 64)
--cookie-samesite lax|none
                     SameSite of the preview cookie; none also sets Secure (default lax)
```

`SANDBOX_LITE_API_TOKEN`, `SANDBOX_LITE_PREVIEW_SECRET` and
`SANDBOX_LITE_TENANT_QUOTA_MB` are read as defaults for those flags. With a
preview secret set, `/api/tenants` returns each tenant's `preview_token`;
opening `http://<id>.<domain>/?sl_token=<token>` once sets a cookie for that
host and redirects to the clean URL. Tokens are per-tenant, so a customer's
link does not open another customer's preview. The cookie is `SameSite=Lax`,
which browsers send only on same-site requests: an editor that embeds previews
from a different registrable domain (`app.example.com` framing
`acme.preview.example.net`) needs `--cookie-samesite none`, which also marks
the cookie `Secure`, so those previews must be served over HTTPS.

In production put the daemon behind a wildcard DNS record (`*.preview.example.com`),
pass `--domain preview.example.com`, set both secrets, and keep `/` and `/api`
reachable only from your own backend. A connection that has not delivered a
complete request head within 30 s — freshly opened or idle between keep-alive
requests — is closed; streaming responses and uploads in progress are not
affected.

### `check`: compile a project without serving it

```sh
sandbox-lite check [--json] examples/starter path/to/other-theme ...
```

Compiles every source file the way the preview would and prints diagnostics
plus a feature census: how many islands, `import.meta.glob` calls, Sass files,
MDX pages, endpoints, integrations and npm imports a project uses. Run it over a
theme catalogue before deciding what the preview must support; the exit code is
non-zero when any file fails to compile, so it doubles as a CI check.

### Docker

```sh
docker build -t sandbox-lite .
docker run -p 4321:4321 -v sandbox-data:/data sandbox-lite
```

The image bundles `examples/` as base projects; mount your own at `/bases`.

## HTTP surface

Editor host (`localhost`):

| Method | Path | |
|---|---|---|
| GET | `/api/stats` | RSS, tenant count, cache stats |
| GET/POST | `/api/tenants` | list / create `{id, base}` |
| DELETE | `/api/tenants/{id}` | remove tenant and its edits |
| GET | `/api/t/{id}/files` | merged base + overlay listing |
| GET/PUT/DELETE | `/api/t/{id}/file/{path}` | raw file bytes |
| GET | `/api/t/{id}/events` | SSE: `update` / `delete` with the new version |
| GET | `/api/t/{id}/check` | compile every source file, return diagnostics |
| POST | `/api/t/{id}/chat` | `{messages:[{role,content}]}` → `{text, changes}` |

Tenant host (`<id>.<domain>`):

| Path | |
|---|---|
| `/` and any page path | shell HTML that renders the matching `src/pages` route in the browser |
| `/__sl/m/<path>?v=N` | compiled ES module (`.astro`, `.ts`, `.tsx`, `.css`, `.scss`, `.json`, `.md`, images; `?raw`, `?url`) |
| `/__sl/m/<file>.astro?astro&type=style&index=i` | one scoped `<style>` block of a component |
| `/__sl/m/<file>.astro?astro&type=script&index=i` | one hoisted `<script>` of a component |
| `/__sl/raw/<path>` | file as-is (assets, `@import`ed CSS) |
| `/__sl/routes.json` | route table built from `src/pages`, in Astro priority order |
| `/__sl/renderers.json` | framework renderers to register, derived from `package.json` (`@astrojs/react`, `@astrojs/preact`) or `sandbox-lite.json` |
| `/__sl/content/<collection>` | `src/content/<collection>/*` as entries with rendered Markdown |
| `/__sl/shim/astro-*.js` | browser stand-ins for `astro:content`, `astro:assets`, `astro:transitions`, … |
| `/__sl/astro.js` | Astro's runtime + container API, bundled once per Astro version |
| `/__sl/events` | same SSE stream as the API; the page reloads itself on it |
| anything under `public/` | served directly |

## How a page renders

1. `GET http://acme.localhost:4321/blog/hello-world` → the daemon answers with a
   short shell page that carries the tenant id, version and `import.meta.env`.
2. `shell.js` fetches `routes.json`, matches the path, and `import()`s
   `/__sl/m/src/pages/blog/[slug].astro?v=…`.
3. The daemon compiles that file with `astro_codegen`, parses the output with
   oxc to find every import specifier, resolves each one against the tenant's
   file tree (extensions, `index` files, `tsconfig` `paths`, `astro:*` shims,
   bare specifiers → CDN) and splices in the URL. The browser follows the graph.
4. `.css` imports and `<style>` blocks become tiny modules that register their
   CSS in a page-level map; the shell injects them into `<head>` before writing
   the document. A stylesheet that `@import "tailwindcss"` is handed to
   Tailwind's browser build.
5. For dynamic routes the shell calls the page's `getStaticPaths()` and picks
   the entry whose params match, exactly as `astro dev` would.
6. `experimental_AstroContainer.renderToResponse()` produces the HTML;
   `document.write` replaces the shell with it, so scripts, links and relative
   URLs behave like a normal page.
7. `live.js` holds an `EventSource`; any write to the tenant reloads the page.

Error states are pages too: a compile error, a missing import, a 404 route or
a failing `getStaticPaths` render an overlay that lists the daemon's
diagnostics and reloads when the file is fixed.

## Layout

```
src/main.rs            CLI, startup
src/store.rs           base projects, tenant overlays, versions, SSE fan-out, persistence
src/resolve.rs         import specifier → URL
src/routes.rs          src/pages → route table
src/transform/         astro (astro_codegen), js (oxc), css, scss (grass), import.meta.glob, markdown, content collections, cache
src/http/              host-based dispatch, auth, editor API, preview endpoints, Claude chat loop
src/check.rs           the `check` subcommand
assets/                shell.html, shell.js, live.js, editor.html, astro.js bundle, astro:* shims
examples/starter       a dependency-free Astro 7 site; the base used by CI's smoke test and bench/mem.sh
examples/tailwind      Tailwind v4 through its browser build
examples/react         a React island hydrated with client:load
scripts/build-runtime.sh   regenerates assets/astro.js for a new Astro version
bench/mem.sh           the memory measurement above
e2e/                   Playwright suite for the browser side: every example page, live reload, hydration, the error overlay
```

## What the preview does not do

- **Vue, Svelte and Solid islands.** Only React and Preact renderers ship;
  another framework needs a `server` module exposing `check` and
  `renderToStaticMarkup` (see `assets/shims/renderer-react.js`, 40 lines) and a
  `client` entrypoint, listed under `renderers` in `sandbox-lite.json`.
- **MDX and endpoints.** `.mdx` pages and `src/pages/*.ts` endpoints show an
  "unsupported route" page. Markdown pages (`.md`, with `layout`) do render.
- **Less/Stylus.** Sass works; other preprocessors are passed through untouched.
- **`content.config.ts` loaders.** Collections are read straight from
  `src/content/<name>/`, which is what the `glob` loader does for almost every
  theme; custom loaders are ignored.
- **`astro.config.*`** is not executed. `site` can be set in `sandbox-lite.json`
  (`{"site": "https://example.com", "imports": {"react": "https://esm.sh/react@19"}}`).
- **Authentication.** There are no users: `--api-token` is one shared bearer
  token for `/api/*`, and `--preview-secret` gates tenant hosts with per-tenant
  links; both are off by default. Put the editor and `/api/*` behind your
  product's own auth and expose only the tenant hosts to customers. See
  `SECURITY.md`.
- **Production builds.** This is the preview. Ship with `astro build` as usual;
  the compiled output is the same compiler, so what you see is what builds.

## License

MIT. `assets/astro.js` is built from the `astro` npm package (MIT); the compiler
is `withastro/compiler-rs` (MIT).
