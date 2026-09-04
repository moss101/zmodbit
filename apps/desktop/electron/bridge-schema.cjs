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
            requireKnownFields(payload, ["title", "prompt"]);
            const title = requireString(payload, "title", MAX_TITLE);
            const prompt = requireString(payload, "prompt", MAX_PROMPT);
            return { kind: "createTask", title, prompt };
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
