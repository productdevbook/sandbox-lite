export const defineMiddleware = (fn) => fn;
export const sequence = (...fns) => fns[0];
export const createContext = () => ({});
export const trySerializeLocals = (v) => JSON.stringify(v);
