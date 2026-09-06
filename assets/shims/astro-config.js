export const defineConfig = (c) => c;
export const getViteConfig = (c) => c;
export const envField = new Proxy({}, { get: () => (o) => o });
export const fontProviders = new Proxy({}, { get: () => (o) => o });
export const mergeConfig = (a, b) => ({ ...a, ...b });
export const validateConfig = (c) => c;
