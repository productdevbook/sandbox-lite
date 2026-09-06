const join = (locale, path = "") => "/" + locale + (path ? (path.startsWith("/") ? path : "/" + path) : "");
export function getRelativeLocaleUrl(locale, path) { return join(locale, path); }
export function getAbsoluteLocaleUrl(locale, path) { return location.origin + join(locale, path); }
export function getRelativeLocaleUrlList(path) { return [join("", path)]; }
export function getAbsoluteLocaleUrlList(path) { return [location.origin + join("", path)]; }
export function getPathByLocale(locale) { return locale; }
export function getLocaleByPath(path) { return path; }
export function redirectToDefaultLocale() {}
export function redirectToFallback() {}
export function notFound() {}
export function requestHasLocale() { return false; }
export const middleware = () => (ctx, next) => next();
