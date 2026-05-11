import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Config } from "../types";

interface UseConfigResult {
  cfg: Config | null;
  setCfg: (next: Config) => void;
  /** Patch one slice of the config and (optionally) auto-save. */
  patch: (mut: (prev: Config) => Config, options?: { save?: boolean }) => void;
  save: () => Promise<void>;
  reload: () => Promise<void>;
  loading: boolean;
  saving: boolean;
  error: string | null;
}

/**
 * Loads ~/.glance/config.toml on mount, exposes a mutable copy, debounces
 * auto-saves when callers pass `{ save: true }` to patch().
 */
export function useConfig(): UseConfigResult {
  const [cfg, setCfgState] = useState<Config | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const saveTimer = useRef<number | null>(null);
  const pending = useRef<Config | null>(null);

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      const c = await invoke<Config>("get_config");
      setCfgState(c);
      setError(null);
    } catch (e) {
      setError(`${e}`);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    reload();
  }, [reload]);

  const save = useCallback(async () => {
    const target = pending.current ?? cfg;
    if (!target) return;
    setSaving(true);
    try {
      await invoke("set_config", { cfg: target });
      setError(null);
    } catch (e) {
      setError(`${e}`);
    } finally {
      setSaving(false);
    }
  }, [cfg]);

  const setCfg = useCallback((next: Config) => {
    setCfgState(next);
    pending.current = next;
  }, []);

  const patch = useCallback(
    (mut: (prev: Config) => Config, options?: { save?: boolean }) => {
      setCfgState((prev) => {
        if (!prev) return prev;
        const next = mut(prev);
        pending.current = next;
        if (options?.save) {
          if (saveTimer.current) window.clearTimeout(saveTimer.current);
          saveTimer.current = window.setTimeout(() => {
            // Pull the latest pending in case more updates landed.
            const target = pending.current ?? next;
            setSaving(true);
            invoke("set_config", { cfg: target })
              .catch((e) => setError(`${e}`))
              .finally(() => setSaving(false));
          }, 300);
        }
        return next;
      });
    },
    [],
  );

  return { cfg, setCfg, patch, save, reload, loading, saving, error };
}
