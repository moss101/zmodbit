// M1.4 end-to-end proof for the desktop's own data path: spawns the real
// `modbit-core` binary, performs the boot-channel + HMAC handshake with the
// JS surface client (node crypto + net), and drives Fleet/New-Task requests.

import { spawn, type ChildProcess } from "node:child_process";
import { existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterAll, describe, expect, it } from "vitest";
import { connectSurface, decodeSurfaceResponse } from "../electron/surface-client.cjs";

const repoRoot = resolve(__dirname, "..", "..", "..");
const coreBin = process.env.MODBIT_CORE_BIN
  ? resolve(process.env.MODBIT_CORE_BIN)
  : join(repoRoot, "target", "debug", process.platform === "win32" ? "modbit-core.exe" : "modbit-core");

const coreBinaryExists = existsSync(coreBin);

interface CoreBoot {
  child: ChildProcess;
  socket: string;
  secret: string;
}

function spawnCore(tag: string): Promise<CoreBoot> {
  const db = join(tmpdir(), `modbit-desktop-${tag}-${Date.now()}-${Math.random().toString(36).slice(2)}.db`);
  const child = spawn(coreBin, [], {
    env: { ...process.env, MODBIT_CORE_DB: db, RUST_LOG: "error" },
    stdio: ["ignore", "pipe", "inherit"],
  });
  return new Promise((resolveBoot, rejectBoot) => {
    let buffered = "";
    const onData = (chunk: Buffer) => {
      buffered += chunk.toString("utf8");
      const newline = buffered.indexOf("\n");
      if (newline >= 0) {
        child.stdout?.off("data", onData);
        const boot = JSON.parse(buffered.slice(0, newline));
        resolveBoot({ child, socket: boot.socket, secret: boot.secret });
      }
    };
    child.stdout?.on("data", onData);
    child.once("exit", (code) => rejectBoot(new Error(`core exited early (${code})`)));
  });
}

describe.skipIf(!coreBinaryExists)("desktop surface client against real modbit-core", () => {
  const spawned: ChildProcess[] = [];

  afterAll(() => {
    for (const child of spawned) child.kill();
  });

  it("authenticates and round-trips get_fleet + create_task", async () => {
    const boot = await spawnCore("e2e");
    spawned.push(boot.child);

    const surface = await connectSurface({ socketPath: boot.socket, secretHex: boot.secret });
    expect(surface.readOnly).toBe(false);
    expect(surface.negotiated).toEqual([1, 0]);

    const created = await surface.request({ createTask: { sessionId: "", title: "From the desktop", prompt: "p" } });
    expect(created.ok).toBe(true);
    expect(created.task?.title).toBe("From the desktop");

    const fleet = await surface.request({ getFleet: {} });
    expect(fleet.ok).toBe(true);
    expect(fleet.fleet?.tasks).toHaveLength(1);
    expect(fleet.fleet?.tasks[0]?.title).toBe("From the desktop");
    expect(fleet.fleet?.defaultSessionId).not.toBe("");

    surface.close();
  });

  it("rejects a wrong boot secret", async () => {
    const boot = await spawnCore("auth");
    spawned.push(boot.child);
    const wrongSecret = "aa".repeat(32);
    await expect(connectSurface({ socketPath: boot.socket, secretHex: wrongSecret })).rejects.toThrow(
      /auth rejected/i,
    );
  });

  it("decodes a canonical SurfaceResponse", () => {
    // Wire-level sanity: ok=true + fleet with one task, hand-encoded.
    // field3 (Fleet): { field1 (TaskView): {1:"t-1", 3:"title", 4:state=6}, field2:"session-0" }
    const taskId = Buffer.from("t-1");
    const title = Buffer.from("title");
    const taskView = Buffer.concat([
      Buffer.from([0x0a, taskId.length]), taskId,
      Buffer.from([0x1a, title.length]), title,
      Buffer.from([0x20, 6]), // field4 varint: COMPLETED
    ]);
    const sessionId = Buffer.from("session-0");
    const fleet = Buffer.concat([
      encodeLenField(1, taskView),
      encodeLenField(2, sessionId),
    ]);
    const response = Buffer.concat([
      Buffer.from([0x08, 0x01]), // ok = true
      encodeLenField(3, fleet),
    ]);
    const decoded = decodeSurfaceResponse(response);
    expect(decoded.ok).toBe(true);
    expect(decoded.fleet.tasks).toHaveLength(1);
    expect(decoded.fleet.tasks[0]).toMatchObject({ taskId: "t-1", title: "title", state: 6 });
    expect(decoded.fleet.defaultSessionId).toBe("session-0");
  });
});

function encodeLenField(fieldNo: number, bytes: Buffer): Buffer {
  const tag = Buffer.from([(fieldNo << 3) | 2]);
  const len = Buffer.from([bytes.length]);
  return Buffer.concat([tag, len, bytes]);
}
