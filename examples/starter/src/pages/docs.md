---
layout: ../layouts/Markdown.astro
title: Editing this site
description: How the live preview works
---

This page is a plain Markdown file (`src/pages/docs.md`) wrapped in `src/layouts/Markdown.astro`.

## What you can edit

- Pages in `src/pages` — `.astro` and `.md` files become routes
- Components in `src/components`, layouts in `src/layouts`
- Styles in `src/styles`, shared data in `src/data`
- Journal posts in `src/content/posts`

## What happens when you save

1. The daemon compiles only the file that changed.
2. Every open preview of this tenant reloads.
3. Your browser renders the page with Astro's own runtime.
