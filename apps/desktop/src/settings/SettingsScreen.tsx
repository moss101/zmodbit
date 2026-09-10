// Settings screen (Phase 4.2, docs/32): the persisted daemon
// configuration — provider, model, base URL, max turns, execution mode —
// with presets for OpenAI, Anthropic and OpenAI-compatible endpoints
// (Ollama/vLLM/z.ai). Every value renders from Core's persisted settings
// (GetSettings) and saves as a partial update (UpdateSettings). Secrets
// NEVER appear here: API keys belong to the secret broker (docs/31).

import { useCallback, useEffect, useState } from "react";
import type { SettingsView } from "@modbit/surface-protocol";

const PRESETS: Record<string, { label: string; baseUrl: string }> = {
  openai: { label: "OpenAI", baseUrl: "https://api.openai.com/v1" },
  anthropic: { label: "Anthropic", baseUrl: "https://api.anthropic.com" },
  "openai-compatible": {
    label: "OpenAI-compatible (Ollama / vLLM / z.ai …)",
    baseUrl: "",
  },
};

const PRESET_BASE_URLS = [
  { label: "Ollama (local)", url: "http://localhost:11434/v1" },
  { label: "vLLM (local)", url: "http://localhost:8000/v1" },
  { label: "z.ai", url: "https://api.z.ai/api/paas/v4" },
];

export function SettingsScreen({ onBack }: { onBack: () => void }) {
  const [settings, setSettings] = useState<SettingsView | null>(null);
  const [provider, setProvider] = useState("openai");
  const [model, setModel] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [maxTurns, setMaxTurns] = useState(8);
  const [executionMode, setExecutionMode] = useState("default");
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [saving, setSaving] = useState(false);
  const [apiKey, setApiKey] = useState("");

  const load = useCallback(async () => {
    try {
      const response = await window.modbit.getSettings();
      if (response.ok && response.settings) {
        const s = response.settings;
        setSettings(s);
        // The stored provider "openai" with a custom base URL renders as
        // the compatible preset so the URL stays editable.
        setProvider(s.provider || "openai");
        setModel(s.model);
        setBaseUrl(s.baseUrl);
        setMaxTurns(s.maxTurns > 0 ? s.maxTurns : 8);
        setExecutionMode(s.executionMode || "default");
        setError(null);
      } else {
        setError(response.error ?? "settings load failed");
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const save = useCallback(async () => {
    setSaving(true);
    setSaved(false);
    try {
      const patch: {
        provider?: string;
        model?: string;
        baseUrl?: string;
        maxTurns?: number;
        executionMode?: string;
        apiKey?: string;
      } = { maxTurns, executionMode };
      // "openai-compatible" stores the openai wire protocol (an
      // OpenAI-compatible endpoint IS OpenAI-wire) + its base URL.
      patch.provider = provider === "anthropic" ? "anthropic" : "openai";
      if (model.trim()) patch.model = model.trim();
      if (provider !== "anthropic" && baseUrl.trim()) patch.baseUrl = baseUrl.trim();
      if (apiKey.trim()) patch.apiKey = apiKey.trim();
      const response = await window.modbit.updateSettings(patch);
      if (response.ok) {
        setSaved(true);
        setApiKey("");
        if (response.settings) setSettings(response.settings);
      } else {
        setError(response.error ?? "settings save failed");
      }
    } finally {
      setSaving(false);
    }
  }, [provider, model, baseUrl, maxTurns, executionMode, apiKey]);

  return (
    <main>
      <h1>Settings</h1>
      <button type="button" onClick={onBack}>
        ← Back to fleet
      </button>
      {error ? <p role="alert">Core error: {error}</p> : null}
      {saved ? <p role="status">Saved — applies to tasks started from now on.</p> : null}

      <section aria-label="Provider">
        <h2>Provider</h2>
        <select
          aria-label="Provider preset"
          value={provider === "anthropic" ? "anthropic" : provider === "openai" && !baseUrl ? "openai" : "openai-compatible"}
          onChange={(e) => {
            const v = e.target.value;
            setProvider(v);
            if (v !== "openai-compatible") setBaseUrl(PRESETS[v]?.baseUrl ?? "");
          }}
        >
          {Object.entries(PRESETS).map(([value, p]) => (
            <option key={value} value={value}>
              {p.label}
            </option>
          ))}
        </select>
        {provider !== "anthropic" ? (
          <>
            <input
              aria-label="Base URL"
              placeholder="https://endpoint/v1"
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
            />
            <select
              aria-label="Compatible endpoint presets"
              value=""
              onChange={(e) => {
                if (e.target.value) setBaseUrl(e.target.value);
              }}
            >
              <option value="">Endpoint presets…</option>
              {PRESET_BASE_URLS.map((p) => (
                <option key={p.url} value={p.url}>
                  {p.label}: {p.url}
                </option>
              ))}
            </select>
          </>
        ) : null}
        <input
          aria-label="Model"
          placeholder="Model id (e.g. gpt-4o-mini, claude-sonnet, glm-4.6)"
          value={model}
          onChange={(e) => setModel(e.target.value)}
        />
      </section>

      <section aria-label="Run budget">
        <h2>Run budget</h2>
        <input
          aria-label="Max turns"
          type="number"
          min={1}
          max={200}
          value={maxTurns}
          onChange={(e) => setMaxTurns(Number(e.target.value))}
        />
      </section>

      <section aria-label="API key">
        <h2>API key</h2>
        <input
          aria-label="API key"
          type="password"
          placeholder={settings?.hasApiKey ? "Stored in the system keychain — enter to replace" : "Paste the provider API key"}
          value={apiKey}
          onChange={(e) => setApiKey(e.target.value)}
        />
        <p className="empty">
          {settings?.hasApiKey
            ? "A key is stored in the system keychain (macOS Keychain / Windows Credential Manager / Secret Service). It never appears here again."
            : "No key stored yet. It will go straight into the OS keychain — never the settings file, environment, or event store."}
        </p>
      </section>
      <section aria-label="Execution mode">
        <h2>Execution mode</h2>
        <select
          aria-label="Execution mode"
          value={executionMode}
          onChange={(e) => setExecutionMode(e.target.value)}
        >
          <option value="default">default — full tool grants</option>
          <option value="readonly">readonly — read-only tools only</option>
        </select>
        <p className="empty">
          readonly tasks refuse edits and shell execution at the capability
          kernel; approval modes expand in Phase 5.
        </p>
      </section>

      <button type="button" disabled={saving} onClick={() => void save()}>
        {saving ? "Saving…" : "Save settings"}
      </button>
      {settings ? (
        <p className="empty">
          Persisted in the daemon's settings store (docs/31); API keys are
          NOT stored here — they come from the secret broker.
        </p>
      ) : null}
    </main>
  );
}
