export const defineMiddleware = (fn) => fn;

// Astro's own `sequence`, minus the rewrite bookkeeping it does between handlers: the container
// resolves a rewrite against the one route it was asked to render, so `next(payload)` is carried to
// the container's own `next` instead (README, "What the preview does not do").
export function sequence(...handlers) {
  const filtered = handlers.filter(Boolean);
  const last = filtered.length - 1;
  if (last < 0) return (_context, next) => next();
  return (context, next) => {
    let carried;
    const apply = (i) =>
      filtered[i](context, async (payload) => {
        if (i === last) return next(payload ?? carried);
        if (payload !== undefined) carried = payload;
        return apply(i + 1);
      });
    return apply(0);
  };
}

export const createContext = () => ({});
export const trySerializeLocals = (v) => JSON.stringify(v);
