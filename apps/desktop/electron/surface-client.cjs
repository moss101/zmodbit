// SurfaceProtocol client for the Electron main process (M1.4, docs/30 § Local
// SurfaceProtocol). Zero dependencies: node crypto + net only.
//
// Wire: 4-byte big-endian length + protobuf bytes. Handshake: Challenge →
// Hello (HMAC-SHA256 over both nonces + protocol version) → AuthResult. After
// auth, frames are SurfaceRequest / SurfaceResponse protobufs.

"use strict";

const crypto = require("node:crypto");
const net = require("node:net");

const PROTOCOL_MAJOR = 1;
const PROTOCOL_MINOR = 0;
const MAX_FRAME_BYTES = 8 * 1024 * 1024;

function encodeVarint(value) {
  // uint64 values arrive as strings from the bindings layer; Number is safe
  // for sequence/generation values below 2^53.
  let v = typeof value === "bigint" ? value : BigInt(value);
  if (v < 0n) throw new Error("negative varint");
  const out = [v === 0n ? 0 : -1];
  // Seed handled explicitly: zero encodes as a single 0x00 byte.
  if (v === 0n) return Buffer.from([0]);
  let index = 0;
  while (v > 0n) {
    let byte = Number(v & 0x7fn);
    v >>= 7n;
    if (v > 0n) byte |= 0x80;
    out[index] = byte;
    index += 1;
  }
  return Buffer.from(out.slice(0, index));
}

function encodeLenField(fieldNo, bytes) {
  const tag = Buffer.from([(fieldNo << 3) | 2]);
  return Buffer.concat([tag, encodeVarint(bytes.length), bytes]);
}

function encodeVarintField(fieldNo, value) {
  return Buffer.concat([Buffer.from([(fieldNo << 3) | 0]), encodeVarint(value)]);
}

function decodeVarint(buf, offset) {
  let result = 0n;
  let shift = 0n;
  let index = offset;
  for (;;) {
    if (index >= buf.length) throw new Error("varint truncated");
    const byte = buf[index];
    index += 1;
    result |= BigInt(byte & 0x7f) << shift;
    if ((byte & 0x80) === 0) break;
    shift += 7n;
  }
  return [result, index];
}


// ---- message encoders ------------------------------------------------------

function encodeCreateSession(displayName) {
  return encodeLenField(1, encodeLenField(1, Buffer.from(displayName, "utf8")));
}

function encodeCreateTask(sessionId, title, prompt) {
  const inner = Buffer.concat([
    encodeLenField(1, Buffer.from(sessionId, "utf8")),
    encodeLenField(2, Buffer.from(title, "utf8")),
    encodeLenField(3, Buffer.from(prompt, "utf8")),
  ]);
  return encodeLenField(2, inner);
}

function encodeGetFleet() {
  return encodeLenField(3, Buffer.alloc(0));
}

function encodeGetTaskEvents(taskId) {
  const inner = encodeLenField(1, Buffer.from(taskId, "utf8"));
  return encodeLenField(8, inner);
}

function encodeSurfaceRequest(request) {
  if (request.createSession !== undefined) return encodeCreateSession(request.createSession);
  if (request.createTask !== undefined) {
    const t = request.createTask;
    return encodeCreateTask(t.sessionId, t.title, t.prompt);
  }
  if (request.taskEvents !== undefined) return encodeGetTaskEvents(request.taskEvents);
  return encodeGetFleet();
}

function decodeTaskEvents(buf) {
  const result = { taskId: "", events: [] };
  for (const [fieldNo, value] of decodeFields(buf)) {
    if (fieldNo === 1) result.taskId = value.toString("utf8");
    else if (fieldNo === 2) {
      // EventEnvelope: event_id(1) str, aggregate_id(3) str, generation(4)
      // varint, event_type(5) str, occurred_at(8) str, payload(7) bytes.
      const event = { eventId: "", aggregateId: "", generation: "0", eventType: "", payload: null };
      for (const [f, v] of decodeFields(value)) {
        if (f === 1) event.eventId = v.toString("utf8");
        else if (f === 3) event.aggregateId = v.toString("utf8");
        else if (f === 4) event.generation = v.toString();
        else if (f === 5) event.eventType = v.toString("utf8");
        else if (f === 7) event.payload = JSON.parse(v.toString("utf8"));
      }
      result.events.push(event);
    }
  }
  return result;
}

// ---- message decoders ------------------------------------------------------

function decodeFields(buf) {
  const fields = [];
  let offset = 0;
  while (offset < buf.length) {
    const [key, next] = decodeVarint(buf, offset);
    const fieldNo = Number(key >> 3n);
    const wireType = Number(key & 7n);
    offset = next;
    if (wireType === 0) {
      const [value, after] = decodeVarint(buf, offset);
      offset = after;
      fields.push([fieldNo, value]);
    } else if (wireType === 2) {
      const [len, afterLen] = decodeVarint(buf, offset);
      const start = Number(afterLen);
      const end = start + Number(len);
      if (end > buf.length) throw new Error("length-delimited field truncated");
      fields.push([fieldNo, buf.subarray(start, end)]);
      offset = end;
    } else {
      throw new Error(`unsupported wire type ${wireType}`);
    }
  }
  return fields;
}

function decodeTaskView(buf) {
  const view = {
    taskId: "",
    sessionId: "",
    title: "",
    state: 0,
    createdAt: "",
    generation: "0",
  };
  for (const [fieldNo, value] of decodeFields(buf)) {
    if (fieldNo === 1) view.taskId = value.toString("utf8");
    else if (fieldNo === 2) view.sessionId = value.toString("utf8");
    else if (fieldNo === 3) view.title = value.toString("utf8");
    else if (fieldNo === 4) view.state = Number(value);
    else if (fieldNo === 5) view.createdAt = value.toString("utf8");
    else if (fieldNo === 6) view.generation = value.toString();
  }
  return view;
}

function decodeFleet(buf) {
  const fleet = { tasks: [], defaultSessionId: "" };
  for (const [fieldNo, value] of decodeFields(buf)) {
    if (fieldNo === 1) fleet.tasks.push(decodeTaskView(value));
    else if (fieldNo === 2) fleet.defaultSessionId = value.toString("utf8");
  }
  return fleet;
}

function decodeSurfaceResponse(buf) {
  const response = {
    ok: false,
    error: "",
    fleet: { tasks: [], defaultSessionId: "" },
    task: null,
    sessionId: "",
    taskEvents: null,
  };
  for (const [fieldNo, value] of decodeFields(buf)) {
    if (fieldNo === 1) response.ok = value !== 0n;
    else if (fieldNo === 2) response.error = value.toString("utf8");
    else if (fieldNo === 3) response.fleet = decodeFleet(value);
    else if (fieldNo === 4) response.task = decodeTaskView(value);
    else if (fieldNo === 5) response.sessionId = value.toString("utf8");
    else if (fieldNo === 6) response.taskEvents = decodeTaskEvents(value);
  }
  return response;
}

// ---- handshake -------------------------------------------------------------

function proofFor(secret, serverNonce, clientNonce, major, minor) {
  const mac = crypto.createHmac("sha256", secret);
  mac.update(serverNonce);
  mac.update(clientNonce);
  const versions = Buffer.alloc(8);
  versions.writeUInt32BE(major, 0);
  versions.writeUInt32BE(minor, 4);
  mac.update(versions);
  return mac.digest();
}

// ---- framed io -------------------------------------------------------------

function readFrame(socket) {
  return new Promise((resolve, reject) => {
    let headerBuf = [];
    let header = null;
    let payload = null;
    let filled = 0;
    const cleanup = () => {
      socket.off("data", onData);
      socket.off("error", onError);
      socket.off("close", onClose);
    };
    const fail = (e) => {
      cleanup();
      reject(e);
    };
    const onData = (chunk) => {
      let cursor = 0;
      while (cursor < chunk.length) {
        if (header === null) {
          const needed = 4 - headerBuf.length;
          const take = Math.min(needed, chunk.length - cursor);
          headerBuf.push(...chunk.subarray(cursor, cursor + take));
          cursor += take;
          if (headerBuf.length === 4) {
            const len = Buffer.from(headerBuf).readUInt32BE(0);
            if (len > MAX_FRAME_BYTES) {
              fail(new Error(`frame of ${len} bytes exceeds ${MAX_FRAME_BYTES}`));
              return;
            }
            header = len;
            payload = Buffer.alloc(len);
            filled = 0;
            if (header === 0) {
              cleanup();
              resolve(payload);
              return;
            }
          }
        } else {
          const needed = header - filled;
          const take = Math.min(needed, chunk.length - cursor);
          chunk.copy(payload, filled, cursor, cursor + take);
          filled += take;
          cursor += take;
          if (filled === header) {
            cleanup();
            resolve(payload);
            return;
          }
        }
      }
    };
    const onError = (e) => fail(e);
    const onClose = () => fail(new Error("connection closed mid-frame"));
    socket.on("data", onData);
    socket.on("error", onError);
    socket.on("close", onClose);
  });
}

function writeFrame(socket, payload) {
  const header = Buffer.alloc(4);
  header.writeUInt32BE(payload.length, 0);
  return new Promise((resolve, reject) => {
    socket.write(Buffer.concat([header, payload]), (e) => (e ? reject(e) : resolve()));
  });
}

// ---- public client ---------------------------------------------------------

async function connectSurface({ socketPath, secretHex }) {
  const secret = Buffer.from(secretHex, "hex");
  // Core reports namespace-style names on Windows ("modbit-core-<id>"); node
  // requires the full local pipe path.
  const address =
    process.platform === "win32" && !socketPath.startsWith("\\\\.\\pipe\\")
      ? `\\\\.\\pipe\\${socketPath}`
      : socketPath;
  const socket = net.createConnection(address);
  await new Promise((resolve, reject) => {
    socket.once("connect", resolve);
    socket.once("error", reject);
  });

  const challenge = await readFrame(socket);
  const fields = decodeFields(challenge);
  const serverNonce = fields.find(([f]) => f === 1)?.[1];
  if (!serverNonce) throw new Error("handshake: missing server nonce");

  const clientNonce = crypto.randomBytes(16);
  const proof = proofFor(secret, serverNonce, clientNonce, PROTOCOL_MAJOR, PROTOCOL_MINOR);
  const hello = Buffer.concat([
    encodeLenField(1, proof),
    encodeLenField(2, clientNonce),
    encodeVarintField(3, PROTOCOL_MAJOR),
    encodeVarintField(4, PROTOCOL_MINOR),
  ]);
  await writeFrame(socket, hello);

  const result = await readFrame(socket);
  // prost omits default-valued fields on the wire — apply proto3 defaults.
  const auth = {
    ok: false,
    readOnly: false,
    negotiatedMajor: 0,
    negotiatedMinor: 0,
    serverVersion: "",
    error: "",
  };
  for (const [fieldNo, value] of decodeFields(result)) {
    if (fieldNo === 1) auth.ok = value !== 0n;
    else if (fieldNo === 2) auth.readOnly = value !== 0n;
    else if (fieldNo === 3) auth.negotiatedMajor = Number(value);
    else if (fieldNo === 4) auth.negotiatedMinor = Number(value);
    else if (fieldNo === 5) auth.serverVersion = value.toString("utf8");
    else if (fieldNo === 6) auth.error = value.toString("utf8");
  }
  if (!auth.ok) {
    socket.destroy();
    throw new Error(`surface auth rejected: ${auth.error}`);
  }

  let pending = Promise.resolve();
  return {
    readOnly: auth.readOnly,
    negotiated: [auth.negotiatedMajor, auth.negotiatedMinor],
    serverVersion: auth.serverVersion,
    request(surfaceRequest) {
      // Serialize request/response pairs on one connection.
      pending = pending.then(async () => {
        await writeFrame(socket, encodeSurfaceRequest(surfaceRequest));
        const response = await readFrame(socket);
        return decodeSurfaceResponse(response);
      });
      return pending;
    },
    close() {
      socket.destroy();
    },
  };
}

module.exports = {
  PROTOCOL_MAJOR,
  PROTOCOL_MINOR,
  MAX_FRAME_BYTES,
  encodeSurfaceRequest,
  encodeCreateTask,
  encodeCreateSession,
  encodeGetFleet,
  decodeSurfaceResponse,
  decodeFleet,
  decodeTaskView,
  decodeFields,
  decodeVarint,
  proofFor,
  connectSurface,
  _internal: { readFrame, writeFrame },
};
