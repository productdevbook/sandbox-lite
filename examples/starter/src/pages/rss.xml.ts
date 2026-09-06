import type { APIContext } from "astro";
import { getCollection } from "astro:content";

const ESCAPES: Record<string, string> = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&apos;" };

function xml(value: string): string {
  return value.replace(/[&<>"']/g, (c) => ESCAPES[c]);
}

export async function GET(context: APIContext): Promise<Response> {
  const base = context.site ?? new URL(context.url.origin);
  const posts = (await getCollection("posts")).sort((a, b) => new Date(b.data.date).getTime() - new Date(a.data.date).getTime());
  const items = posts
    .map((post) => {
      const link = new URL(`/blog/${post.id}`, base).href;
      return `    <item>
      <title>${xml(post.data.title)}</title>
      <link>${xml(link)}</link>
      <guid isPermaLink="true">${xml(link)}</guid>
      <description>${xml(post.data.description)}</description>
      <pubDate>${new Date(post.data.date).toUTCString()}</pubDate>
    </item>`;
    })
    .join("\n");
  const body = `<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Lumen Studio</title>
    <link>${xml(base.href)}</link>
    <description>Notes on design, process and the tools we build with.</description>
${items}
  </channel>
</rss>
`;
  return new Response(body, { headers: { "content-type": "application/xml; charset=utf-8" } });
}
