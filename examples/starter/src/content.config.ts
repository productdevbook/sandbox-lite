import { defineCollection, z } from "astro:content";
import { file, glob } from "astro/loaders";

const posts = defineCollection({
  loader: glob({ pattern: "**/*.{md,mdx}", base: "./src/content/posts" }),
  schema: z.object({
    title: z.string(),
    description: z.string(),
    date: z.coerce.date(),
    tags: z.array(z.string()).default([]),
  }),
});

const team = defineCollection({
  loader: file("src/content/team.json"),
  schema: z.object({
    name: z.string(),
    role: z.string(),
    bio: z.string(),
    joined: z.coerce.date(),
  }),
});

export const collections = { posts, team };
