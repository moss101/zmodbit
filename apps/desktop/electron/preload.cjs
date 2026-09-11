// Preload bridge (docs/32 § Security settings): exposes ONLY typed
// SurfaceProtocol functions to the sandboxed renderer. No node APIs, no
// shell/file access, no arbitrary surfaces.

"use strict";

const { contextBridge, ipcRenderer } = require("node:electron");

contextBridge.exposeInMainWorld("modbit", {
  fleetSnapshot: () => ipcRenderer.invoke("fleet:snapshot"),
  createTask: (title, prompt, repoId = "", baseBranch = "") =>
    ipcRenderer.invoke("task:create", { title, prompt, repoId, baseBranch }),
  runVariants: (objective, count, repoId = "", baseBranch = "") =>
    ipcRenderer.invoke("fleet:runVariants", { objective, count, repoId, baseBranch }),
  listRecentRepos: () => ipcRenderer.invoke("repo:list"),
  registerRepo: (path, cloneUrl) =>
    ipcRenderer.invoke("repo:register", { path: path || "", cloneUrl: cloneUrl || "" }),
  getSettings: () => ipcRenderer.invoke("settings:get"),
  updateSettings: (patch) => ipcRenderer.invoke("settings:update", patch),
  // api_key rides the SAME guarded channel; the schema guard bounds its
  // length and the host routes it to the OS keychain — never to logs.
  updateApiKey: (apiKey) =>
    ipcRenderer.invoke("settings:update", { apiKey }),
  createSession: (displayName) => ipcRenderer.invoke("session:create", { displayName }),
  taskEvents: (taskId) => ipcRenderer.invoke("task:events", { taskId }),
  codeView: (path) => ipcRenderer.invoke("code:view", { path }),
  runDetail: (taskId) => ipcRenderer.invoke("task:runDetail", { taskId }),
  diff: (taskId) => ipcRenderer.invoke("task:diff", { taskId }),
  steerTask: (taskId, note) => ipcRenderer.invoke("task:steer", { taskId, note }),
  pauseTask: (taskId) => ipcRenderer.invoke("task:pause", { taskId }),
  stopTask: (taskId, reason) => ipcRenderer.invoke("task:stop", { taskId, reason }),
  // Core event stream (docs/30 SubscribeEvents): the main process owns the
  // SSE connection; the renderer receives offset-corrected events only.
  onCoreEvent: (listener) => {
    const handler = (_event, payload) => listener(payload);
    ipcRenderer.on("modbit:event", handler);
    return () => ipcRenderer.off("modbit:event", handler);
  },
  onTaskEvent: (listener) => {
    const handler = (_event, payload) => {
      if (payload?.aggregateId) listener(payload);
    };
    ipcRenderer.on("modbit:event", handler);
    return () => ipcRenderer.off("modbit:event", handler);
  },
});
