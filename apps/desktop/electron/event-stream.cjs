// Core daemon SSE subscription (docs/30 § SubscribeEvents; docs/33 §
// backpressure). The MAIN process owns the stream (the sandboxed renderer
// never makes network calls); events are forwarded over IPC. Pure helpers
// are exported for tests.

"use strict";

/** Parses one SSE chunk into {data} payloads, preserving partial lines. */
function createSseParser(onData) {
  let buffer = "";
  return {
    feed(chunk) {
      buffer += chunk;
      let index;
      while ((index = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, index).replace(/\r$/, "");
        buffer = buffer.slice(index + 1);
        if (line.startsWith("data:")) {
          const payload = line.slice(5).trim();
          if (payload) {
            try {
              onData(JSON.parse(payload));
            } catch {
              // Ignore malformed frames; the offset cursor keeps us correct.
            }
          }
        }
      }
    },
  };
}

/** Extracts the daemon bound address from a core stderr line. */
function parseDaemonAddr(stderrLine) {
  const m = /modbit-core: http daemon on (\S+)/.exec(stderrLine || "");
  return m ? m[1] : null;
}

/**
 * Subscribes to `GET /events?since=<offset>` on the daemon and calls
 * onEvent(event) + onOffset(offset) as frames arrive. Returns a stop()
 * that aborts the fetch. Reconnects with the last offset on stream end.
 */
function subscribeEvents(addr, { onEvent, onOffset, signal }) {
  let offset = 0;
  let stopped = false;
  const controller = new AbortController();
  const onAbort = () => controller.abort();
  signal?.addEventListener("abort", onAbort, { once: true });

  async function loop() {
    while (!stopped) {
      try {
        const response = await fetch(`http://${addr}/events?since=${offset}`, {
          signal: controller.signal,
        });
        if (!response.ok || !response.body) throw new Error(`events http ${response.status}`);
        const parser = createSseParser((event) => {
          if (typeof event.sequence === "number") {
            offset = Math.max(offset, event.sequence);
            onOffset(offset);
          }
          onEvent(event);
        });
        const reader = response.body.getReader();
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          parser.feed(new TextDecoder().decode(value));
        }
      } catch (e) {
        if (stopped || controller.signal.aborted) return;
        // Stream ended or dropped: reconnect from the last offset after a
        // bounded pause (docs/33: replay from cursor is lossless).
        await new Promise((r) => setTimeout(r, 500));
      }
    }
  }
  void loop();
  return {
    stop() {
      stopped = true;
      controller.abort();
      signal?.removeEventListener("abort", onAbort);
    },
  };
}

module.exports = { createSseParser, parseDaemonAddr, subscribeEvents };
