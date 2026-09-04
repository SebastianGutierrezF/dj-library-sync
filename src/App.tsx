import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import type { DetectedFile, LocalTrack, MixKind, ScanResult } from "./types";

const DERIVATIVE: MixKind[] = ["Remix", "Rework", "Edit", "Vip"];

function descriptorLabel(track: LocalTrack): string {
  const { kind, remixer } = track.parsed;
  if (remixer && DERIVATIVE.includes(kind)) return `${remixer} ${kind.toLowerCase()}`;
  if (kind === "None") return "—";
  return kind.toLowerCase();
}

function duration(ms: number): string {
  const total = Math.floor(ms / 1000);
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, "0")}`;
}

export default function App() {
  const [folder, setFolder] = useState<string | null>(null);
  const [tracks, setTracks] = useState<LocalTrack[]>([]);
  const [freshPaths, setFreshPaths] = useState<Set<string>>(new Set());
  const [failures, setFailures] = useState<ScanResult["failures"]>([]);
  const [watching, setWatching] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // A file that finished downloading arrives here, already tag-parsed.
  useEffect(() => {
    const unlisten = listen<DetectedFile>("track-detected", (event) => {
      const { track, path, error: readError } = event.payload;
      if (!track) {
        setFailures((prev) => [{ path, error: readError ?? "unreadable" }, ...prev]);
        return;
      }
      setTracks((prev) => [track, ...prev.filter((t) => t.path !== track.path)]);
      setFreshPaths((prev) => new Set(prev).add(track.path));
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  const chooseFolder = useCallback(async () => {
    const selected = await open({ directory: true, multiple: false, title: "Pick your downloads folder" });
    if (typeof selected !== "string") return;

    setBusy(true);
    setError(null);
    try {
      const result = await invoke<ScanResult>("scan", { path: selected, recursive: true });
      setFolder(selected);
      setTracks(result.tracks);
      setFailures(result.failures);
      setFreshPaths(new Set());

      await invoke("start_watching", { path: selected });
      setWatching(true);
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  }, []);

  const toggleWatching = useCallback(async () => {
    if (!folder) return;
    try {
      if (watching) {
        await invoke("stop_watching");
        setWatching(false);
      } else {
        await invoke("start_watching", { path: folder });
        setWatching(true);
      }
    } catch (err) {
      setError(String(err));
    }
  }, [folder, watching]);

  const stats = useMemo(() => {
    const total = tracks.length;
    const withIsrc = tracks.filter((t) => t.isrc).length;
    const withVersion = tracks.filter((t) => t.parsed.kind !== "None").length;
    return { total, withIsrc, withVersion };
  }, [tracks]);

  return (
    <div className="app">
      <header>
        <div>
          <h1>DJ Library Sync</h1>
          <p className="folder">{folder ?? "No folder selected"}</p>
        </div>
        <div className="actions">
          {folder && (
            <button className="ghost" onClick={toggleWatching}>
              {watching ? "Pause watching" : "Resume watching"}
            </button>
          )}
          <button onClick={chooseFolder} disabled={busy}>
            {folder ? "Change folder" : "Choose folder"}
          </button>
        </div>
      </header>

      {error && <div className="banner error">{error}</div>}

      {folder && (
        <div className="stats">
          <Stat label="tracks" value={stats.total} />
          <Stat
            label="with ISRC"
            value={stats.total ? `${Math.round((stats.withIsrc / stats.total) * 100)}%` : "—"}
            hint="the high-confidence match path"
          />
          <Stat label="version tagged" value={stats.withVersion} />
          <Stat
            label={watching ? "watching" : "paused"}
            value={watching ? "live" : "off"}
            tone={watching ? "good" : "muted"}
          />
        </div>
      )}

      {!folder ? (
        <div className="empty">
          <p>Pick the folder your downloads land in.</p>
          <p className="dim">
            Files are read only once they stop growing, so half-written downloads never get parsed.
          </p>
        </div>
      ) : tracks.length === 0 ? (
        <div className="empty">
          <p>No audio files here yet.</p>
          <p className="dim">Drop one in — it should appear within a few seconds.</p>
        </div>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Artist</th>
              <th>Title</th>
              <th>Version</th>
              <th className="num">Length</th>
              <th className="num">ISRC</th>
            </tr>
          </thead>
          <tbody>
            {tracks.map((track) => (
              <tr key={track.path}>
                <td>
                  {freshPaths.has(track.path) && <span className="badge">new</span>}
                  {track.artist || <span className="dim">untagged</span>}
                </td>
                <td>{track.parsed.base}</td>
                <td className={track.parsed.kind === "None" ? "dim" : "version"}>
                  {descriptorLabel(track)}
                </td>
                <td className="num">{duration(track.duration_ms)}</td>
                <td className="num">{track.isrc ? "yes" : <span className="dim">—</span>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {failures.length > 0 && (
        <details className="failures">
          <summary>{failures.length} file(s) could not be read</summary>
          <ul>
            {failures.slice(0, 20).map((f) => (
              <li key={f.path}>
                <code>{f.path}</code>
                <span className="dim"> — {f.error}</span>
              </li>
            ))}
          </ul>
        </details>
      )}
    </div>
  );
}

function Stat({
  label,
  value,
  hint,
  tone,
}: {
  label: string;
  value: string | number;
  hint?: string;
  tone?: "good" | "muted";
}) {
  return (
    <div className={`stat ${tone ?? ""}`}>
      <span className="value">{value}</span>
      <span className="label">{label}</span>
      {hint && <span className="hint">{hint}</span>}
    </div>
  );
}
