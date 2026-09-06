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
pages with `layout:`; MDX pages and MDX collection entries (compiled
in-process with Astro's `satteri-mdxjs`, rendered with Astro's JSX runtime);
endpoints (`src/pages/rss.xml.ts`, `src/pages/api/*.json.ts`: the `GET` handler
runs in the browser and its `Response` is shown);
dynamic routes with `getStaticPaths` and `paginate`;
`import.meta.glob` (eager and lazy, `import:`/`query:` options); `import.meta.env`
and `.env` `PUBLIC_*` variables; `tsconfig` path aliases; npm packages from a
CDN pinned to the versions in `package.json`; React, Preact, Vue and Svelte
islands (`client:load` and friends hydrate with the framework's own runtime,
whether the component is a file in `src/` or one imported from a package, and
`.vue`/`.svelte` single-file components are compiled in the browser, with the
TypeScript in their `<script>` blocks stripped by the daemon first);
`<script>` tags, `public/` files and live reload.

## What it costs

`bench/mem.sh` starts the release binary, creates N tenants from the starter
project, edits one file in each, loads the preview module graph of each, and
reads the daemon's RSS from `/proc`.

Release build on Linux x86-64, three base projects loaded, every tenant with
one edited page and a loaded preview:

| | daemon RSS |
|---|---|
| idle | 7.0 MB |
| after 1000 tenants (edit + preview each) | 19.7 MB — 13 kB per tenant |

Creating a tenant, writing its edit and fetching its whole module graph took
~147 ms per tenant through the HTTP API. After 1000 tenants the transform cache
held 1008 entries in 571 kB (1000 unique edited pages plus the 8 shared modules
every tenant reuses), 7993 hits to 1008 misses. The binary is 15.8 MB.

### The same project, the other way

`bench/vs-astro-dev.sh` runs the comparison on one machine: `npm install` plus
`astro dev` for one tenant, then the daemon serving the same project.

| | `astro dev`, one per tenant | sandbox-lite, one for all |
|---|---|---|
| dependencies | 4.1 s install, 166 MB of `node_modules` **per tenant** | none — the browser fetches packages from a CDN, already built |
| server ready | 3.0 s | 8 ms |
| tenant ready | (the process *is* the tenant) | 6 ms |
| first page, whole module graph | 97 ms | 51 ms (6 modules compiled) |
| **cold → first page** | **7.2 s** | **65 ms** |
| memory | 636 MB for that one tenant | 12 MB for the daemon and every tenant in it |

The install was measured with a warm npm cache; in a fresh container it was
18 s, which is the number that matters — that is what a hosted builder pays
every time it starts a session. For scale: ComputeSDK's public
[sandbox benchmark](https://www.computesdk.com/benchmarks/sandboxes/dax/) times
32 providers doing clone + install + typecheck in a fresh sandbox, and the
fastest finishes in 33.5 s.

sandbox-lite is not in that benchmark and cannot be: it has no shell and runs no
customer code. It removes the install step rather than accelerating it, which
only works because the compiler is a library and the runtime is the browser's.
That trade is the whole design — see *What the preview does not do*.

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
the preview updates — a stylesheet swaps into the open page, anything else
reloads it.

With `ANTHROPIC_API_KEY` set, the Chat tab talks to Claude with `read_file`,
`write_file`, `delete_file`, `list_files` and `check_site` tools scoped to that
tenant. Every write shows up in the preview as it happens. The reply is
streamed: the editor sends `Accept: text/event-stream` and paints the text and
each tool call as they arrive. Conversations are kept per tenant under
`<data-dir>/<id>/chats/` and are listed in the Chat tab's dropdown, so a
customer can pick an old thread back up; the tool loop sees the earlier turns.

A conversation does not grow without end. The last `--chat-window` turns
(default 24) are replayed to the model in full; when a conversation passes that,
everything older is folded into one summary — written once by the same API,
stored with the conversation and sent as its opening turn from then on. Should
that call fail, the turns stay stored and the window is applied to the request
anyway. Conversations are outside the tenant quota, so `--chats-per-tenant`
(default 50) is what bounds them: saving past it drops the tenant's least
recently updated conversation.

`--chrome PATH` adds a sixth tool, `screenshot`, which renders one page of the
tenant's live preview in headless Chrome and hands the PNG back to the model,
so it can look at the layout it just changed instead of guessing. A browser
costs more memory than every tenant on the daemon put together, so
`--chrome-jobs` (default 1) of them run at once; a call that has not got a slot
within 10 s is told the tool is busy rather than queueing, and one that overruns
its 20 s deadline is killed and reaped before its profile directory goes.

### Flags

```
--listen ADDR        bind address (default 127.0.0.1:4321)
--domain NAME        tenants are served at http://<id>.NAME:PORT/ (default localhost)
--bases DIR          directory whose sub-directories are base projects (symbolic links are skipped)
--base NAME=PATH     add one base project (repeatable)
--watch-bases N      re-read a base project when its files change, polled every N seconds (default: off)
--data-dir DIR       where tenant edits are persisted (default ./data)
--no-persist         keep edits in memory only
--cdn URL            where bare npm imports resolve in the browser (default https://esm.sh)
--cache-mb N         transform cache budget (default 64)
--max-source-kb N    largest .astro/.ts/.js/.mdx/.scss/.vue/.svelte file the compilers accept (default 64)
--sass-timeout-ms N  deadline for one Sass compile (default 5000)
--max-compiles N     compiles that may run at once (default: one per core)
--model NAME         Claude model for the chat endpoint (default claude-fable-5-1)
--api-token TOKEN    require `Authorization: Bearer TOKEN` (or `?token=`) on /api/*
--preview-secret S   tenant hosts require a per-tenant token derived from S
--chrome PATH        chrome or chromium binary for the chat's screenshot tool (default: off)
--chrome-jobs N      screenshots that may run at once (default 1)
--chat-window N      turns of a conversation replayed to the model in full (default 24)
--chats-per-tenant N conversations a tenant may keep (default 50)
--tenant-quota-mb N  edited files a tenant may hold, in MiB (default 64)
--cookie-samesite lax|none
                     SameSite of the preview cookie; none also sets Secure (default lax)
```

`SANDBOX_LITE_API_TOKEN`, `SANDBOX_LITE_PREVIEW_SECRET`,
`SANDBOX_LITE_TENANT_QUOTA_MB`, `SANDBOX_LITE_MAX_COMPILES`,
`SANDBOX_LITE_CHROME`, `SANDBOX_LITE_CHROME_JOBS`, `SANDBOX_LITE_CHAT_WINDOW`
and `SANDBOX_LITE_CHATS_PER_TENANT` are read as defaults for those flags, and
`SANDBOX_LITE_ANTHROPIC_BASE` points the chat at another Messages API
endpoint (default `https://api.anthropic.com`). With a
preview secret set, `/api/tenants` returns each tenant's `preview_token`;
`?sl_token=<token>` is accepted on every request, so the token alone opens a
preview, and every response that carries one also sets the `sl_t` cookie. A
top-level navigation is redirected to the clean URL; a framed one is served
where it is, keeping the token, because a redirect would drop it from the URL
before the page had it. With a preview secret the cookie defaults to
`SameSite=None; Secure` — the only form a browser stores for a cross-site frame
— which browsers accept over HTTPS and on loopback (`localhost`, `127.0.0.1`
and `*.localhost`); pass `--cookie-samesite lax` if the previews are same-site
with the editor. The cookie is what carries the `/favicon.svg` a layout writes
by hand and the imports inside a compiled module, which no URL rewriting
reaches. A request with neither a valid token nor the cookie answers `403`,
always: no request header decides access. Tokens are per-tenant, so a
customer's link does not open another customer's preview.

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
| GET | `/metrics` | Prometheus text format: gauges, cache and request counters, compile-latency histograms |
| GET | `/api/stats` | RSS, tenant count, cache stats, what the chats directory holds, screenshots in flight, module requests and compile totals |
| GET/POST | `/api/bases` | list / add `{name, path}`; the path must be inside `--bases` when that flag is set |
| POST | `/api/bases/{name}/reload` | re-read the base from disk and re-point every tenant on it |
| GET/POST | `/api/tenants` | list / create `{id, base}` |
| DELETE | `/api/tenants/{id}` | remove the tenant: its map entry and its whole `<data-dir>/{id}` directory, whether or not the daemon had it loaded |
| POST | `/api/tenants/{id}/import` | tar.gz body → overlay; `?replace=1` drops the edits it does not carry |
| GET | `/api/t/{id}/files` | merged base + overlay listing |
| GET | `/api/t/{id}/export` | tar.gz of the merged tree; `?overlay=1` for the edits alone |
| GET/PUT/DELETE | `/api/t/{id}/file/{path}` | raw file bytes |
| GET | `/api/t/{id}/events` | SSE: `update` / `delete` with the new version and a `kind` (`css`, `style`, `module`) |
| GET | `/api/t/{id}/check` | compile every source file, return diagnostics |
| POST | `/api/t/{id}/chat` | `{messages:[{role,content}], chat?}` → `{text, changes, chat}`, or SSE with `Accept: text/event-stream` |
| GET | `/api/t/{id}/chats` | saved conversations, newest first |
| GET/DELETE | `/api/t/{id}/chats/{chat}` | one conversation with its turns / remove it |

`export` answers `application/gzip` with a `<id>.tar.gz` attachment name. By
default it holds the merged tree — the base project with the tenant's edits
applied and its deleted files left out — which is what `astro build` wants.
A merged export carries no record of what the tenant deleted, so importing one
into another tenant on the same base leaves that tenant's copy of a deleted
file in place. `?overlay=1` holds only the tenant's own files plus a
`.sandbox-lite/deleted.json` listing the base files it deleted; `import`
applies that list rather than writing the file, so an overlay export
reproduces a tenant exactly on another daemon with the same base. An import
writes every entry through the tenant quota, bumps the version once and emits
one `update` event, whatever the file count.

```sh
curl -fsS localhost:4321/api/t/acme/export?overlay=1 > acme.tar.gz
curl -fsS -X POST --data-binary @acme.tar.gz localhost:4321/api/tenants/acme-copy/import
```

`/metrics` is scrapeable as it is — no exporter, no client library. It carries
`sandbox_lite_tenants`, `sandbox_lite_overlay_bytes`, `sandbox_lite_cache_*`,
`sandbox_lite_sse_subscribers`, `sandbox_lite_rss_bytes`,
`sandbox_lite_uptime_seconds`, one series per base, `sandbox_lite_sass_*`
(running, runaway, timeouts, refusals), `sandbox_lite_screenshots_*` (running,
calls told the tool was busy, browsers killed on their deadline),
`sandbox_lite_module_requests_total{kind,status}` and
`sandbox_lite_compile_seconds{kind}` — a histogram of what a transform-cache
miss costs, so `histogram_quantile(0.99, …)` answers "how slow is a cold
compile" and `rate(sandbox_lite_cache_misses_total[5m])` answers "how often".
It is behind `--api-token` like the rest of the editor host; a scrape then
needs `bearer_token` in the job.

Tenant host (`<id>.<domain>`):

| Path | |
|---|---|
| `/` and any page path | shell HTML that renders the matching `src/pages` route in the browser |
| `/__sl/m/<path>?v=N` | compiled ES module (`.astro`, `.ts`, `.tsx`, `.css`, `.scss`, `.json`, `.md`, `.mdx`, `.vue`, `.svelte`, images; `?raw`, `?url`) |
| `/__sl/m/<file>.astro?astro&type=style&index=i` | one scoped `<style>` block of a component |
| `/__sl/m/<file>.astro?astro&type=script&index=i` | one hoisted `<script>` of a component |
| `/__sl/raw/<path>` | file as-is (assets, `@import`ed CSS) |
| `/__sl/routes.json` | route table built from `src/pages`, in Astro priority order |
| `/__sl/renderers.json` | framework renderers to register, derived from `package.json` (`@astrojs/react`, `@astrojs/preact`, `@astrojs/vue`, `@astrojs/svelte`) or `sandbox-lite.json` |
| `/__sl/content/<collection>` | `{entries, dates}` — the collection's entries and the schema's date fields; Markdown arrives rendered, MDX entries render through their compiled module |
| `/__sl/shim/astro-*.js` | browser stand-ins for `astro:content`, `astro:assets`, `astro:transitions`, …; `astro-jsx-runtime.js` is Astro's JSX runtime and `astro:jsx` renderer |
| `/__sl/astro.js` | Astro's runtime + container API, bundled once per Astro version |
| `/__sl/events` | same SSE stream as the API; the page swaps CSS or reloads itself on it |
| anything under `public/` | served directly |

## Updating a base project

Base projects are read into memory at startup, so a theme fix on disk does not
reach the tenants built on it by itself. Two ways to make it:

```sh
curl -fsS -X POST localhost:4321/api/bases/starter/reload
# {"name":"starter","files":24,"bytes":38104,"root":"/srv/themes/starter","tenants":["acme","bakery"]}

sandbox-lite --bases /srv/themes --watch-bases 5   # or poll for it
```

Either one re-walks the base directory, swaps the result in, and re-points
every tenant on that base — each of which gets a version bump and one `update`
event, so open previews reload. A tenant's own edits are untouched: the overlay
still wins over the base copy of a file it has edited, and an edit over a file
the reload deleted stays visible. The transform cache needs no invalidation,
being keyed by content: a changed file simply hashes to a new key.

`--watch-bases N` polls each base root every N seconds, comparing the name,
size and mtime of every file `Base::load` would take — one walk per base per
interval, no inotify, no new dependency. A rewrite that keeps both the size and
the mtime is not noticed; the reload endpoint is the escape hatch for that.

`POST /api/bases {"name","path"}` adds a base to a running daemon. With
`--bases` set the path must resolve inside that directory; without it, any
readable directory on the host will do — the API token is the only gate, so
this is an operator surface. The startup scan applies the same rule: an entry
of `--bases` becomes a base only when the entry itself is a directory, so a
symbolic link is skipped rather than followed, and `--base NAME=PATH` is held
to the containment too. See `SECURITY.md`.

Bases and reloads are per process. Running more than one daemon behind a load
balancer with a shared `--data-dir` — what each node does and does not see, and
the two configurations that work — is [`docs/multi-node.md`](docs/multi-node.md).

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
7. A `.ts`/`.js` file under `src/pages` is an endpoint: the same call with
   `routeType: "endpoint"` runs its `GET` (or `ALL`) handler with an
   `APIContext`. An HTML response is written as a page; anything else — XML,
   JSON, plain text — is shown with its status and content-type above the body.
8. `live.js` holds an `EventSource`. Each event carries a `kind`: a `.css`,
   `.scss` or `.sass` write is `css`, an `.astro` write whose compiled JS is
   byte-identical to the last build changed only its `<style>` blocks and is
   `style`, and everything else is `module`. On `css` and `style` the CSS
   module is re-imported and the matching `<style data-sl=…>` swapped in place,
   leaving the page's DOM and JS state alone; on `module` — or on anything the
   daemon cannot prove, or while the error overlay is up — the page reloads.
   Tailwind is the exception: a `type="text/tailwindcss"` block is compiled by
   Tailwind's browser build when it loads and there is no rebuild hook to call,
   so a write that touches one reloads the page.

Error states are pages too: a compile error, a missing import, a 404 route or
a failing `getStaticPaths` render an overlay that lists the daemon's
diagnostics and reloads when the file is fixed.

## Layout

```
src/main.rs            CLI, startup
src/store.rs           base projects, tenant overlays, versions, SSE fan-out, persistence
src/resolve.rs         import specifier → URL
src/routes.rs          src/pages → route table
src/transform/         astro (astro_codegen), js (oxc), css, scss (grass), sfc (Vue/Svelte script blocks), import.meta.glob, markdown, mdx (satteri-mdxjs), content collections, cache
src/http/              host-based dispatch, auth, editor API, preview endpoints, Claude chat loop, stream framing, saved conversations
src/check.rs           the `check` subcommand
src/metrics.rs         atomic counters and the Prometheus text-format renderer behind /metrics
assets/                shell.html, shell.js, live.js, editor.html, astro.js and astro-jsx.js bundles, astro:* shims
examples/starter       a framework-free Astro 7 site (Markdown, MDX, content collections, endpoints); the base used by CI's smoke test and bench/mem.sh
examples/tailwind      Tailwind v4 through its browser build
examples/react         React islands hydrated with client:load, one local and one imported from npm
examples/vue           a TypeScript Vue SFC compiled in the browser and hydrated with client:load
examples/svelte        the same for a Svelte 5 component
scripts/build-runtime.sh   regenerates assets/astro.js and assets/astro-jsx.js for a new Astro version
bench/mem.sh           the memory measurement above
docs/multi-node.md     two daemons behind a load balancer over one --data-dir: what each node sees, and the routing that works
e2e/                   Playwright suite for the browser side: every example page, endpoints, live reload, hydration, the error overlay
```

## What the preview does not do

- **Solid islands.** React, Preact, Vue and Svelte renderers ship; another
  framework needs a `server` module exposing `check` and
  `renderToStaticMarkup` (see `assets/shims/renderer-react.js`, 40 lines) and a
  `client` entrypoint, listed under `renderers` in `sandbox-lite.json`.
- **Type-driven Vue macros.** There is no Rust compiler for `.vue` or
  `.svelte`, so the daemon serves the component as a loader module that runs
  `@vue/compiler-sfc` or `svelte/compiler` in the browser and imports the result
  as a blob module. Every `<script lang="ts">` block is stripped to JavaScript
  by the daemon before that, which leaves `defineProps<Props>()`,
  `defineEmits`, `defineModel`, `defineSlots` and `<script setup generic="…">`
  with no type to compile — they are refused with a diagnostic asking for the
  runtime form. A blob has no import map either, so the loader rewrites
  `vue`/`svelte` imports to the CDN and relative ones against the component's
  own URL: `./Other.vue` works, `./other` (no extension) does not.
- **Non-`GET` requests.** A visitor navigates, so only an endpoint's `GET` (or
  `ALL`) handler ever runs and the request carries no body. `src/middleware.ts`
  is not loaded, and `astro:actions` answers `SERVICE_UNAVAILABLE`.
- **Less/Stylus.** Sass works; other preprocessors are passed through untouched.
- **`content.config.ts` loaders.** The config is read statically, never
  executed: `glob({ pattern, base })` and `file(path)` with literal arguments
  are honoured, and `z.date()` / `z.coerce.date()` fields in a `z.object({…})`
  schema are revived as `Date` exactly. A loader the reader cannot see through
  — a custom one, or a pattern built from a variable — falls back to reading
  `src/content/<name>/`; a schema it cannot see through falls back to reviving
  every ISO-shaped string.
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

MIT. `assets/astro.js` and `assets/astro-jsx.js` are built from the `astro` and
`@astrojs/mdx` npm packages (MIT); the compiler is `withastro/compiler-rs` (MIT).
