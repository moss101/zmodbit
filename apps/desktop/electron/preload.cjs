// Preload bridge (docs/32 § Security settings): exposes ONLY typed
// SurfaceProtocol functions to the sandboxed renderer. No node APIs, no
// shell/file access, no arbitrary surfaces.

"use strict";

const { contextBridge, ipcRenderer } = require("node:electron");

contextBridge.exposeInMainWorld("modbit", {
  fleetSnapshot: () => ipcRenderer.invoke("fleet:snapshot"),
  createTask: (title, prompt) => ipcRenderer.invoke("task:create", { title, prompt }),
  createSession: (displayName) => ipcRenderer.invoke("session:create", { displayName }),
  taskEvents: (taskId) => ipcRenderer.invoke("task:events", { taskId }),
});
