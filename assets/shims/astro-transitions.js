import { createComponent, render } from "/__sl/astro.js";
export const ClientRouter = createComponent(() => render`<meta name="astro-view-transitions-enabled" content="true">`, "astro:transitions/ClientRouter");
export const ViewTransitions = ClientRouter;
export const fade = (o = {}) => ({ forwards: {}, backwards: {}, ...o });
export const slide = (o = {}) => ({ forwards: {}, backwards: {}, ...o });
