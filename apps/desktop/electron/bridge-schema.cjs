// Renderer→bridge schema guard (M1, REQ-EV-0103): every message crossing the
// preload boundary is validated against a strict schema BEFORE it reaches the
// privileged host. A malicious renderer message without schema/capability is
// rejected — this module is the enforcement point (QUAL-EV-0103).
//
// Rules:
// - channel allowlist (no dynamic channels);
// - payloads must be plain objects created by the renderer (no null, no
//   arrays at the root);
// - no prototype-pollution keys anywhere in the payload;
// - per-field type + length bounds;
// - unknown fields are rejected, not ignored.

"use strict";

const MAX_TITLE = 200;
const MAX_PROMPT = 20_000;
const MAX_DISPLAY_NAME = 100;
const MAX_QUESTION = 5_000;

const FORBIDDEN_KEYS = new Set(["__proto__", "constructor", "prototype"]);

class Rejected extends Error {
    constructor(reason) {
        super(`bridge rejected: ${reason}`);
        this.name = "BridgeRejected";
    }
}

function isPlainObject(value) {
    if (value === null || typeof value !== "object" || Array.isArray(value)) return false;
    const proto = Object.getPrototypeOf(value);
    return proto === Object.prototype || proto === null;
}

function rejectIfForbiddenKeys(value, depth = 0) {
    if (depth > 4) throw new Rejected("payload nesting exceeds 4 levels");
    if (Array.isArray(value)) {
        for (const item of value) rejectIfForbiddenKeys(item, depth + 1);
        return;
    }
    if (value !== null && typeof value === "object") {
        for (const key of Object.keys(value)) {
            if (FORBIDDEN_KEYS.has(key)) throw new Rejected(`forbidden key "${key}"`);
            rejectIfForbiddenKeys(value[key], depth + 1);
        }
    }
}

function requireString(obj, field, maxLen) {
    const value = obj[field];
    if (typeof value !== "string") throw new Rejected(`${field} must be a string`);
    if (value.length === 0) throw new Rejected(`${field} must not be empty`);
    if (value.length > maxLen) throw new Rejected(`${field} exceeds ${maxLen} characters`);
    return value;
}

function optionalString(obj, field, maxLen) {
    const value = obj[field];
    if (value === undefined) return "";
    if (typeof value !== "string") throw new Rejected(`${field} must be a string`);
    if (value.length > maxLen) throw new Rejected(`${field} exceeds ${maxLen} characters`);
    return value;
}

function requireKnownFields(obj, allowed) {
    for (const key of Object.keys(obj)) {
        if (!allowed.includes(key)) throw new Rejected(`unknown field "${key}"`);
    }
}

const CHANNELS = {
    "fleet:snapshot": {
        validate(payload) {
            if (payload === undefined) return { kind: "fleetSnapshot" };
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, []);
            return { kind: "fleetSnapshot" };
        },
    },
    "task:create": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["title", "prompt", "repoId", "baseBranch"]);
            const title = requireString(payload, "title", MAX_TITLE);
            const prompt = requireString(payload, "prompt", MAX_PROMPT);
            const repoId = optionalString(payload, "repoId", 64);
            const baseBranch = optionalString(payload, "baseBranch", 200);
            return { kind: "createTask", title, prompt, repoId, baseBranch };
        },
    },
    "approval:approve": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["approvalId", "resolvedBy"]);
            const approvalId = requireString(payload, "approvalId", 200);
            const resolvedBy = optionalString(payload, "resolvedBy", 200);
            return { kind: "approveEffect", approvalId, resolvedBy };
        },
    },
    "approval:deny": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["approvalId", "reason", "resolvedBy"]);
            const approvalId = requireString(payload, "approvalId", 200);
            const reason = optionalString(payload, "reason", 2000);
            const resolvedBy = optionalString(payload, "resolvedBy", 200);
            return { kind: "denyEffect", approvalId, reason, resolvedBy };
        },
    },
    "browser:view": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["taskId"]);
            const taskId = requireString(payload, "taskId", 200);
            return { kind: "getBrowserView", taskId };
        },
    },
    "browser:lease": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["taskId", "owner"]);
            const taskId = requireString(payload, "taskId", 200);
            const owner = requireString(payload, "owner", 8);
            if (owner !== "agent" && owner !== "user") {
                throw new Rejected("owner must be agent or user");
            }
            return { kind: "setBrowserLease", taskId, owner };
        },
    },
    "approval:list": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, []);
            return { kind: "listPendingApprovals" };
        },
    },
    "fleet:runVariants": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["objective", "count", "repoId", "baseBranch"]);
            const objective = requireString(payload, "objective", MAX_PROMPT);
            const count = payload.count;
            if (!Number.isInteger(count) || count < 2 || count > 4) {
                throw new Rejected("count must be an integer between 2 and 4");
            }
            const repoId = optionalString(payload, "repoId", 64);
            const baseBranch = optionalString(payload, "baseBranch", 200);
            return { kind: "runVariants", objective, count, repoId, baseBranch };
        },
    },
    "repo:register": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["path", "cloneUrl"]);
            const path = optionalString(payload, "path", 512);
            const cloneUrl = optionalString(payload, "cloneUrl", 500);
            if (path && cloneUrl) throw new Rejected("provide exactly one of path or cloneUrl");
            if (!path && !cloneUrl) throw new Rejected("provide exactly one of path or cloneUrl");
            if (path.includes("..")) throw new Rejected("path traversal rejected");
            return { kind: "registerRepo", path, cloneUrl };
        },
    },
    "settings:get": {
        validate(payload) {
            if (payload === undefined) return { kind: "getSettings" };
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, []);
            return { kind: "getSettings" };
        },
    },
    "settings:update": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, [
              "provider",
              "model",
              "baseUrl",
              "maxTurns",
              "executionMode",
              "apiKey",
            ]);
            const patch = {};
            if (payload.provider !== undefined) {
                const v = requireString(payload, "provider", 40).toLowerCase();
                if (!["openai", "anthropic"].includes(v) && v !== "openai-compatible") {
                    throw new Rejected("provider must be openai, anthropic or openai-compatible");
                }
                patch.provider = v === "openai-compatible" ? "openai" : v;
            }
            if (payload.model !== undefined) patch.model = requireString(payload, "model", 200);
            if (payload.baseUrl !== undefined) {
                const u = optionalString(payload, "baseUrl", 300);
                if (u && !u.startsWith("http://") && !u.startsWith("https://")) {
                    throw new Rejected("baseUrl must be an http(s) URL");
                }
                patch.baseUrl = u;
            }
            if (payload.maxTurns !== undefined) {
                if (!Number.isInteger(payload.maxTurns) || payload.maxTurns < 1 || payload.maxTurns > 200) {
                    throw new Rejected("maxTurns must be an integer 1..200");
                }
                patch.maxTurns = payload.maxTurns;
            }
            if (payload.executionMode !== undefined) {
                const m = requireString(payload, "executionMode", 40).toLowerCase();
                if (!["default", "readonly", "approvals"].includes(m)) {
                    throw new Rejected("executionMode must be default, readonly or approvals");
                }
                patch.executionMode = m;
            }
            if (payload.apiKey !== undefined) {
                const k = optionalString(payload, "apiKey", 400);
                patch.apiKey = k;
            }
            return { kind: "updateSettings", ...patch };
        },
    },
    "repo:list": {
        validate(payload) {
            if (payload === undefined) return { kind: "repoList" };
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, []);
            return { kind: "repoList" };
        },
    },
    "task:events": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["taskId"]);
            const taskId = requireString(payload, "taskId", 64);
            return { kind: "taskEvents", taskId };
        },
    },
    "code:view": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["path"]);
            const path = requireString(payload, "path", 512);
            if (path.includes("..")) throw new Rejected("path traversal rejected");
            return { kind: "codeView", path };
        },
    },
    "task:runDetail": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["taskId"]);
            const taskId = requireString(payload, "taskId", 64);
            return { kind: "runDetail", taskId };
        },
    },
    "task:diff": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["taskId"]);
            const taskId = requireString(payload, "taskId", 64);
            return { kind: "diff", taskId };
        },
    },
    "task:steer": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["taskId", "note"]);
            const taskId = requireString(payload, "taskId", 64);
            const note = requireString(payload, "note", MAX_PROMPT);
            return { kind: "steer", taskId, note };
        },
    },
    "task:pause": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["taskId"]);
            const taskId = requireString(payload, "taskId", 64);
            return { kind: "pause", taskId };
        },
    },
    "task:stop": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["taskId", "reason"]);
            const taskId = requireString(payload, "taskId", 64);
            const reason = payload.reason === undefined ? "" : payload.reason;
            if (typeof reason !== "string" || reason.length > MAX_PROMPT) {
                throw new Rejected("reason must be a bounded string");
            }
            return { kind: "stop", taskId, reason };
        },
    },
    "session:create": {
        validate(payload) {
            if (!isPlainObject(payload)) throw new Rejected("payload must be an object");
            requireKnownFields(payload, ["displayName"]);
            const displayName = requireString(payload, "displayName", MAX_DISPLAY_NAME);
            return { kind: "createSession", displayName };
        },
    },
};

/// Validates a renderer message. Returns the normalized, typed request for
/// the privileged host or throws `Rejected`.
function validateIpcMessage(channel, payload) {
    const schema = CHANNELS[channel];
    if (!schema) throw new Rejected(`unknown channel "${channel}"`);
    if (payload !== undefined && !isPlainObject(payload)) {
        throw new Rejected("payload must be a plain object");
    }
    return schema.validate(payload === undefined ? {} : payload);
}

module.exports = { validateIpcMessage, Rejected, CHANNELS, MAX_TITLE, MAX_PROMPT };
