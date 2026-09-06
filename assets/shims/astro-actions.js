export class ActionError extends Error {
  constructor({ message = "action error", code = "INTERNAL_SERVER_ERROR" } = {}) { super(message); this.code = code; }
}
const unavailable = async () => ({ data: undefined, error: new ActionError({ message: "Actions are not available inside the sandbox-lite preview", code: "SERVICE_UNAVAILABLE" }) });
export const actions = new Proxy({}, { get: () => Object.assign(unavailable, { orThrow: async () => { throw new ActionError(); }, queryString: "" }) });
export function defineAction(c) { return c; }
export function isInputError() { return false; }
export function isActionError(e) { return e instanceof ActionError; }
export function getActionState() { return undefined; }
export function getActionContext() { return { action: undefined }; }
export function deserializeActionResult(r) { return r; }
export function serializeActionResult(r) { return r; }
export function getActionPath() { return "/_actions"; }
