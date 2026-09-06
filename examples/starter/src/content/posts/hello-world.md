---
title: Why we render previews in the browser
description: A dev server per customer does not scale. The browser already has a JavaScript engine.
date: 2026-08-14
tags: [engineering, astro]
---

## The problem

Every hosted site builder that offers "edit with AI, see it live" ends up with the same bill:
one `astro dev` process per customer, each holding a few hundred megabytes of Node.js, Vite and
compiler state, most of it idle.

## The trick

The compiler for `.astro` files is a native library. The runtime that turns a compiled component
into HTML is plain JavaScript that runs anywhere. So the split is simple:

- a small native daemon compiles files on demand and hands out ES modules,
- the visitor's browser imports those modules and renders the page itself.

```ts
const container = await AstroContainer.create();
const html = await container.renderToString(Page, { props });
```

Nothing about the customer's page ever runs on the server.
