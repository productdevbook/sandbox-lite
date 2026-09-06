# Several daemons behind a load balancer

Two sandbox-lite processes can share one `--data-dir` on NFS, EFS or an S3
mount. What follows is what actually happens when they do, and the two
configurations that work today. Every claim is about the code in `src/` as it
is; where it says "does not", that is a limit, not a plan.

## What a daemon holds in memory

A daemon is not a stateless front end over the data directory. Each one holds:

- **Base projects**, read from disk at startup (`Base::load`) and re-read only
  on `POST /api/bases/{name}/reload` or a `--watch-bases` tick. Each daemon
  reads its own copy; nothing is shared.
- **Tenants**, each an `Arc<Base>` plus an overlay
  (`BTreeMap<String, Option<FileData>>`) built from `<data-dir>/<id>/files` and
  `deleted.json` at startup (`Store::restore`), or on the first lookup that
  misses (below). After that the overlay is the daemon's own copy: it is
  written through to disk, and never re-read.
- **A version per tenant**, milliseconds since the epoch, bumped on every
  write. It is not persisted: a restart, or a restore on another node, produces
  a fresh one.
- **An SSE channel per tenant** (`tokio::sync::broadcast`, 64 slots) that only
  that daemon's own writes publish to.

Conversations are the exception, and the only shared state here. With a
`--data-dir`, `Chats` holds nothing: every load, save, list and delete goes
straight to `<data-dir>/<id>/chats/*.json` (`src/http/chats.rs`), so a chat
started on one node is readable on the other as soon as that node has the
tenant. Two nodes saving the same chat id is last-writer-wins like any other
file. With `--no-persist` they are per-node memory and are not shared at all.

So with two daemons, A and B, sharing `--data-dir`:

| | What happens |
|---|---|
| `PUT /api/t/acme/file/src/pages/index.astro` on A | A's overlay and `<data-dir>/acme/files/…` both change. B's overlay does not. |
| A preview open against B | B still serves its own copy of the file, at its own version. It gets no event, so it does not reload. |
| `GET /api/t/acme/files` on B | B's view: the files it knows about, at B's version. |
| A tenant created on A, then a request for it on B | B has no such tenant in memory, so it restores one from `<data-dir>/acme` and serves it (`Store::tenant`). |
| A writes again after B has restored | B does not see it. The restore happens once, on the miss. |
| `DELETE /api/tenants/acme` on A | The directory goes; B keeps serving the tenant it already holds in memory. |
| `POST /api/bases/theme/reload` on A | Only A re-reads the base. B keeps the copy it loaded. |
| `POST /api/t/acme/chat` on A, then `GET /api/t/acme/chats` on B | B lists it: conversations are read from the shared data directory on every call, not cached. |
| `POST /api/tenants/acme/import` on A | A's overlay and the data directory change; B's overlay does not, exactly as for a single write. |
| `/api/stats`, `/metrics` on B | B's own tenants, cache and subscribers. Neither number is a cluster total. |

The version being per node also means a browser that moves from A to B
mid-session gets a lower `?v=` than it had. That is harmless — the daemon never
reads the value, it only busts the browser cache — but the two nodes' numbers
are not comparable, and neither is a total.

Nothing here is corruption: writes go through the tenant's write lock on the
node that serves them, and each file write is a whole-file `std::fs::write`.
Two nodes writing the same file at the same time is last-writer-wins on the shared
filesystem, with each node's memory holding what it wrote. The problem is
staleness, not tearing.

## Configuration 1: sticky routing by tenant host

Route on the `Host` header — `acme.preview.example.com` always to the same
node — and every write for a tenant lands on the node that serves it. This is
the configuration to use.

```nginx
# hash on the first label of the preview host, so one tenant is always one node
map $host $tenant { ~^(?<label>[^.]+)\.preview\.example\.com$ $label; default $host; }

upstream previews {
    hash $tenant consistent;
    server 10.0.0.1:4321;
    server 10.0.0.2:4321;
}

server {
    server_name ~^[^.]+\.preview\.example\.com$;
    location / { proxy_pass http://previews; proxy_set_header Host $host; }
}
```

The editor and `/api/*` answer on every host that is *not* a tenant host
(`SECURITY.md`), so they need the same treatment: an API call about `acme` must
reach the node serving `acme`. Route `/api/t/{id}/…` and
`/api/tenants/{id}/…` by that `{id}` with the same hash, and the writes and the
preview stay on one node.

What still bites:

- **A node coming or going reshuffles the hash.** With `hash … consistent` only
  a share of tenants move, and a moved tenant is restored from the data
  directory on its new node — with whatever the old node had already written to
  disk, which is everything it acknowledged. What is lost is the version and
  the open SSE connections: previews reconnect and reload.
- **Two nodes can end up holding the same tenant** if routing changes twice, or
  if an API call is routed by a different key than the preview. Then the two
  memories diverge silently. Restarting one node is the cure; there is nothing
  in the daemon that detects it.
- **`POST /api/bases` and `POST /api/bases/{name}/reload` are per node.** Call
  them on every node, or give every node `--watch-bases N` over a shared base
  directory.
- **`--chats-per-tenant` is enforced per call, against the shared directory.**
  Two nodes saving conversations for one tenant at the same time can each
  evict what the other just wrote; the cap holds, which conversation survives
  is a race.

## Configuration 2: one node per base

Give each daemon its own `--bases` (or its own `--base NAME=PATH`) and route by
base rather than by tenant: node A serves the `shop` theme, node B the `blog`
theme. A tenant's base is in `<data-dir>/<id>/tenant.json`, and a node skips
any tenant whose base it has not loaded — at startup with a message, and on a
lookup miss silently — so the tenants of another node's bases are simply not
served, which is what you want here.

This is how to scale a catalogue of themes past what one process should hold in
memory: two daemons, disjoint base sets, one data directory, and the routing
decision made where the tenant is created.

## What would be needed for real multi-node

Not implemented, listed so nobody has to rediscover it:

1. **Invalidation.** A node must learn that another node wrote to a tenant it
   holds — a message bus, or a version file in the tenant directory that a
   reader checks and, when it is ahead, re-reads the overlay from disk.
2. **A shared version.** Persisting the version per tenant, and taking the
   maximum across nodes, so `?v=` and the `hello` event mean the same thing
   everywhere.
3. **SSE fan-out across nodes.** Today `live.js` reloads only on the events of
   the node holding its connection.
4. **Base coordination.** One reload call, or one watcher, that every node
   follows.

Until then: route stickily, keep one tenant on one node, and treat
`--data-dir` as durability rather than as shared state — conversations being
the one thing it does share.
