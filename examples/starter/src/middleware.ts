import { defineMiddleware, sequence } from "astro:middleware";

// One place decides what every page and endpoint can read off Astro.locals.
const studio = defineMiddleware((context, next) => {
  context.locals.studio = "Lumen Studio";
  return next();
});

// The members area is behind a link the studio hands out; everyone else is sent to the about page.
const members = defineMiddleware((context, next) => {
  if (context.url.pathname === "/members" && context.url.searchParams.get("token") !== "lumen") {
    return context.redirect("/about", 302);
  }
  return next();
});

export const onRequest = sequence(studio, members);
