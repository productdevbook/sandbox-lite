const chain = new Proxy(function () {}, {
  get: (_, key) => (key === Symbol.toPrimitive || key === "then" ? undefined : chain),
  apply: () => chain,
  construct: () => chain,
});
export const z = chain;
export default chain;
