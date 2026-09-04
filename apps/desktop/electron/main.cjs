// Electron main process — the ONLY desktop process permitted to connect to
// Core (docs/30 § Local SurfaceProtocol; docs/32 § Security settings).
//
// Boot: spawn the modbit-core binary, read the boot channel json line
// (`{"socket", "secret"}`) from its inherited stdout, authenticate over the
// local transport, then expose typed SurfaceProtocol functions to the
// sandboxed renderer through the preload bridge.

"use strict";

const { app, BrowserWindow, ipcMain } = require("electron");
const { spawn } = require("node:child_process");
const path = require("node:path");
const fs = require("node:fs");
const { connectSurface } = require("./surface-client.cjs");
const { validateIpcMessage, Rejected } = require("./bridge-schema.cjs");

let coreProcess = null;
let surface = null;

function coreBinaryPath() {
  if (process.env.MODBIT_CORE_BIN) return process.env.MODBIT_CORE_BIN;
  const exe = process.platform === "win32" ? "modbit-core.exe" : "modbit-core";
  return path.join(__dirname, "..", "..", "..", "target", "debug", exe);
}

async function startCore() {
  const dbDir = process.env.MODBIT_CORE_DB;
  const args = [];
  if (dbDir) args.push("--db", dbDir);
  coreProcess = spawn(coreBinaryPath(), args, {
    env: { ...process.env, RUST_LOG: "error" },
    stdio: ["ignore", "pipe", "inherit"],
  });
  const bootLine = await new Promise((resolve, reject) => {
    let buffered = "";
    const onData = (chunk) => {
      buffered += chunk.toString("utf8");
      const newline = buffered.indexOf("\n");
      if (newline >= 0) {
        coreProcess.stdout.off("data", onData);
        resolve(JSON.parse(buffered.slice(0, newline)));
      }
    };
    coreProcess.stdout.on("data", onData);
    coreProcess.once("exit", (code) => reject(new Error(`core exited early (${code})`)));
  });
  return bootLine;
}

async function connectToCore() {
  const boot = await startCore();
  surface = await connectSurface({ socketPath: boot.socket, secretHex: boot.secret });
  console.log(`surface connected: ${surface.serverVersion} readOnly=${surface.readOnly}`);
}

async function withRetry(fn) {
  if (!surface) await connectToCore();
  try {
    return await fn(surface);
  } catch (e) {
    // Core restarts recover from the durable store; reconnect and retry once.
    surface = null;
    await connectToCore();
    return fn(surface);
  }
}

async function getFleet() {
  return withRetry((s) => s.request({ getFleet: {} }));
}

async function getTaskEvents(taskId) {
  return withRetry((s) => s.request({ taskEvents: taskId }));
}

async function getCodeView(path) {
  return withRetry((s) => s.request({ codeView: path }));
}

async function createTask({ title, prompt }) {
  return withRetry((s) => s.request({ createTask: { sessionId: "", title, prompt } }));
}

async function createSession({ displayName }) {
  return withRetry((s) => s.request({ createSession: displayName }));
}

// REQ-EV-0103: every renderer message is schema-validated before the
// privileged host acts on it. Malicious/malformed messages are rejected.
function guarded(channel, action) {
  ipcMain.handle(channel, (_event, payload) => {
    let request;
    try {
      request = validateIpcMessage(channel, payload);
    } catch (e) {
      if (e instanceof Rejected) return { ok: false, error: e.message };
      throw e;
    }
    return action(request);
  });
}

function registerIpc() {
  guarded("fleet:snapshot", () => getFleet());
  guarded("task:create", (request) => {
    if (request.kind !== "createTask") return { ok: false, error: "wrong request kind" };
    return createTask({ title: request.title, prompt: request.prompt });
  });
  guarded("session:create", (request) => {
    if (request.kind !== "createSession") return { ok: false, error: "wrong request kind" };
    return createSession({ displayName: request.displayName });
  });
  guarded("task:events", (request) => {
    if (request.kind !== "taskEvents") return { ok: false, error: "wrong request kind" };
    return getTaskEvents(request.taskId);
  });
  guarded("code:view", (request) => {
    if (request.kind !== "codeView") return { ok: false, error: "wrong request kind" };
    return getCodeView(request.path);
  });
}

function createWindow() {
  const win = new BrowserWindow({
    width: 1280,
    height: 800,
    title: "Modbit",
    webPreferences: {
      nodeIntegration: false,
      contextIsolation: true,
      sandbox: true,
      preload: path.join(__dirname, "preload.cjs"),
    },
  });
  win.loadFile(path.join(__dirname, "..", "dist", "index.html"));
}

app.whenReady().then(async () => {
  registerIpc();
  try {
    await connectToCore();
  } catch (e) {
    console.error("core connection failed:", e.message);
  }
  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});

app.on("before-quit", () => {
  if (coreProcess && coreProcess.exitCode === null) coreProcess.kill();
});
