import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import type {
  AccountStatus,
  AppConfig,
  Candidate,
  DetectedFile,
  LocalTrack,
  MatchRow,
  MixKind,
  PlatformOption,
  PlaylistInfo,
  PushResult,
} from "./types";

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

function defaultPlaylistName(): string {
  const now = new Date();
  const pad = (n: number) => String(n).padStart(2, "0");
  return `New Downloads ${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
}

export default function App() {
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [account, setAccount] = useState<AccountStatus | null>(null);
  const [clientIdDraft, setClientIdDraft] = useState("");
  const [redirect, setRedirect] = useState<string>("");
  const [platforms, setPlatforms] = useState<PlatformOption[]>([]);
  const [connecting, setConnecting] = useState<string | null>(null);
  /** Files the watcher has seen land since the last match. */
  const [pending, setPending] = useState<string[]>([]);

  const [rows, setRows] = useState<MatchRow[]>([]);
  const [progress, setProgress] = useState<{ done: number; total: number } | null>(null);
  const [chosen, setChosen] = useState<Record<number, string>>({});
  const [selected, setSelected] = useState<Set<number>>(new Set());

  const [playlists, setPlaylists] = useState<PlaylistInfo[]>([]);
  const [playlistName, setPlaylistName] = useState("");
  const [result, setResult] = useState<PushResult | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refreshAccount = useCallback(async () => {
    setAccount(await invoke<AccountStatus>("account_status"));
    setPlatforms(await invoke<PlatformOption[]>("available_platforms"));
  }, []);

  const [bootError, setBootError] = useState<string | null>(null);

  useEffect(() => {
    (async () => {
      try {
        const cfg = await invoke<AppConfig>("load_config");
        setConfig(cfg);
        setPlaylistName(cfg.last_playlist ?? defaultPlaylistName());
        try {
          setRedirect(await invoke<string>("redirect_uri"));
        } catch {
          /* no client id yet */
        }
        await refreshAccount();
      } catch (err) {
        // Without this the window sits on "Loading…" forever with no clue why.
        setBootError(String(err));
      }
    })();

    const unlistenProgress = listen<{ done: number; total: number }>("match-progress", (e) =>
      setProgress(e.payload)
    );

    // The watcher is the whole point of the app: downloads should show up
    // without anyone opening anything. The backend emits one of these per file
    // once it has stopped growing.
    const unlistenDetected = listen<DetectedFile>("track-detected", (e) => {
      const name = e.payload.track?.file_name ?? e.payload.path;
      setPending((prev) => (prev.includes(name) ? prev : [...prev, name]));
    });

    return () => {
      void unlistenProgress.then((fn) => fn());
      void unlistenDetected.then((fn) => fn());
    };
  }, [refreshAccount]);

  const persist = useCallback(async (next: AppConfig) => {
    setConfig(next);
    await invoke("save_config", { config: next });
  }, []);

  const saveClientId = useCallback(async () => {
    if (!config || !clientIdDraft.trim()) return;
    await persist({ ...config, spotify_client_id: clientIdDraft.trim() });
    try {
      setRedirect(await invoke<string>("redirect_uri"));
    } catch {
      /* ignore */
    }
    await refreshAccount();
  }, [config, clientIdDraft, persist, refreshAccount]);

  const signIn = useCallback(async () => {
    setBusy("Waiting for Spotify in your browser…");
    setError(null);
    try {
      await invoke("spotify_login");
      await refreshAccount();
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(null);
    }
  }, [refreshAccount]);

  const chooseFolder = useCallback(async () => {
    if (!config) return;
    const picked = await open({ directory: true, multiple: false, title: "Pick your downloads folder" });
    if (typeof picked !== "string") return;
    await persist({ ...config, watch_folder: picked });
    await invoke("start_watching", { path: picked }).catch(() => undefined);
  }, [config, persist]);

  const runMatch = useCallback(
    async (rescan: boolean) => {
      if (!config?.watch_folder) return;
      setBusy(rescan ? "Re-querying Spotify…" : "Matching…");
      setError(null);
      setResult(null);
      try {
        const found = await invoke<MatchRow[]>("match_folder", {
          path: config.watch_folder,
          acceptShorter: config.accept_shorter,
          rescan,
        });
        setRows(found);
        setPending([]);
        // Confident matches are pre-selected; everything else waits for a
        // decision, which is the entire point of the verdict split.
        setSelected(new Set(found.filter((r) => r.verdict === "auto").map((r) => r.track_id)));
        setChosen(
          Object.fromEntries(
            found.flatMap((r) => (r.candidates[0] ? [[r.track_id, r.candidates[0].track.uri]] : []))
          )
        );
      } catch (err) {
        setError(String(err));
      } finally {
        setBusy(null);
        setProgress(null);
      }
    },
    [config]
  );

  const loadPlaylists = useCallback(async () => {
    try {
      setPlaylists(await invoke<PlaylistInfo[]>("list_playlists"));
    } catch (err) {
      setError(String(err));
    }
  }, []);

  const push = useCallback(async () => {
    const items = rows
      .filter((r) => selected.has(r.track_id))
      .map((r) => {
        const uri = chosen[r.track_id];
        const c = r.candidates.find((x) => x.track.uri === uri);
        return {
          track_id: r.track_id,
          uri,
          // Sent so the backend can spot the same recording under a
          // different Spotify URI, not just an identical one.
          name: c?.track.name ?? "",
          artists: c?.track.artists.join(", ") ?? "",
          duration_ms: c?.track.duration_ms ?? 0,
        };
      })
      .filter((i) => Boolean(i.uri));

    if (items.length === 0 || !config) return;

    setBusy(`Adding ${items.length} track(s)…`);
    setError(null);
    try {
      const res = await invoke<PushResult>("push_tracks", { playlistName, items });
      setResult(res);
      await persist({ ...config, last_playlist: playlistName });
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(null);
    }
  }, [rows, selected, chosen, playlistName, config, persist]);

  const stats = useMemo(() => {
    const by = (v: string) => rows.filter((r) => r.verdict === v).length;
    return { total: rows.length, auto: by("auto"), review: by("review"), missing: by("no_match") };
  }, [rows]);

  const readyToPush = rows.filter((r) => selected.has(r.track_id) && chosen[r.track_id]).length;

  if (bootError) {
    return (
      <div className="app">
        <h1>DJ Library Sync</h1>
        <div className="banner error">Could not start: {bootError}</div>
        <p className="dim">
          This build talks to a Tauri backend. Opening the dev server directly in
          a browser shows the interface but cannot reach it.
        </p>
      </div>
    );
  }

  if (!config) return <div className="app"><p className="dim">Loading…</p></div>;

  // --- Connect a service --------------------------------------------------
  if (!account?.configured || !account?.signed_in) {
    return (
      <div className="app">
        <h1>DJ Library Sync</h1>
        <p className="dim intro">Pick where your new downloads should end up.</p>

        {error && <div className="banner error">{error}</div>}

        <div className="services">
          {platforms.map((p) => (
            <div key={p.id} className={`service ${p.available ? "" : "soon"}`}>
              <div className="service-head">
                <div>
                  <h2>{p.display_name}</h2>
                  <p className="dim small">
                    {!p.available
                      ? "Coming soon"
                      : p.credentials === "user_provided"
                      ? "Free — uses your own developer app"
                      : "One-click sign in"}
                  </p>
                </div>
                {p.connected ? (
                  <span className="pill good">connected</span>
                ) : p.available ? (
                  <button
                    onClick={() =>
                      setConnecting(connecting === p.id ? null : p.id)
                    }
                  >
                    {connecting === p.id ? "Close" : "Connect"}
                  </button>
                ) : (
                  <span className="pill">{p.metered ? "paid" : "free"}</span>
                )}
              </div>

              {connecting === p.id && p.credentials === "user_provided" && (
                <div className="setup">
                  <p className="dim">
                    Spotify only lets a developer app serve five people, so this one
                    runs on an app you own. It takes about two minutes and it is free.
                  </p>
                  <ol className="dim">
                    <li>
                      Open <code>developer.spotify.com/dashboard</code> and create an app
                    </li>
                    <li>
                      Add <code>{redirect || "http://127.0.0.1:8888/callback"}</code> as a
                      Redirect URI — it must be the <code>127.0.0.1</code> form,
                      Spotify rejects <code>localhost</code>
                    </li>
                    <li>Under Settings → User Management, add your own Spotify account</li>
                    <li>Copy the Client ID and paste it below</li>
                  </ol>
                  <p className="dim small">
                    Only the Client ID — never the secret. This uses PKCE, which is
                    designed so desktop apps do not need one.
                  </p>
                  <div className="row">
                    <input
                      value={clientIdDraft}
                      onChange={(e) => setClientIdDraft(e.target.value)}
                      placeholder="Client ID"
                      spellCheck={false}
                    />
                    <button
                      onClick={account?.configured ? signIn : saveClientId}
                      disabled={!account?.configured && !clientIdDraft.trim()}
                    >
                      {account?.configured ? "Sign in" : "Save"}
                    </button>
                  </div>
                </div>
              )}
            </div>
          ))}
        </div>
      </div>
    );
  }

  return (
    <div className="app">
      <header>
        <div>
          <h1>DJ Library Sync</h1>
          <p className="folder">{config.watch_folder ?? "No folder selected"}</p>
        </div>
        <div className="actions">
          {account.signed_in ? (
            <span className="who">{account.display_name ?? account.user_id}</span>
          ) : (
            <button onClick={signIn}>Connect Spotify</button>
          )}
          <button className="ghost" onClick={chooseFolder}>
            {config.watch_folder ? "Change folder" : "Choose folder"}
          </button>
        </div>
      </header>

      {busy && <div className="banner">{busy}{progress ? ` ${progress.done}/${progress.total}` : ""}</div>}

      {!busy && pending.length > 0 && (
        <div className="banner good arrivals">
          <span>
            {pending.length} new {pending.length === 1 ? "download" : "downloads"} landed
            <span className="dim small"> — {pending.slice(-3).join(", ")}</span>
          </span>
          <button onClick={() => runMatch(false)}>Match them</button>
        </div>
      )}
      {error && <div className="banner error">{error}</div>}
      {result && (
        <div className="banner good">
          Added {result.added} to “{result.playlist_name}”
          {result.skipped > 0 && ` · ${result.skipped} already there`}
        </div>
      )}

      {account.signed_in && config.watch_folder && (
        <>
          <div className="stats">
            <Stat label="tracks" value={stats.total} />
            <Stat label="ready" value={stats.auto} tone="good" />
            <Stat label="needs a look" value={stats.review} />
            <Stat label="not found" value={stats.missing} tone="muted" />
          </div>

          <div className="toolbar">
            <button onClick={() => runMatch(false)} disabled={!!busy}>Match new downloads</button>
            <button className="ghost" onClick={() => runMatch(true)} disabled={!!busy}>
              Re-check everything
            </button>
            <label className="check">
              <input
                type="checkbox"
                checked={config.accept_shorter}
                onChange={(e) => persist({ ...config, accept_shorter: e.target.checked })}
              />
              Accept the shorter cut when the extended mix isn’t on Spotify
            </label>
          </div>
        </>
      )}

      {rows.length > 0 && (
        <>
          <table>
            <thead>
              <tr>
                <th className="tick"></th>
                <th>Track</th>
                <th>Version</th>
                <th>Match on Spotify</th>
                <th className="num">Conf.</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <RowView
                  key={row.track_id}
                  row={row}
                  checked={selected.has(row.track_id)}
                  chosenUri={chosen[row.track_id]}
                  onToggle={() =>
                    setSelected((prev) => {
                      const next = new Set(prev);
                      next.has(row.track_id) ? next.delete(row.track_id) : next.add(row.track_id);
                      return next;
                    })
                  }
                  onChoose={(uri) => setChosen((prev) => ({ ...prev, [row.track_id]: uri }))}
                />
              ))}
            </tbody>
          </table>

          <div className="pushbar">
            <input
              list="playlists"
              value={playlistName}
              onChange={(e) => setPlaylistName(e.target.value)}
              onFocus={loadPlaylists}
              placeholder="Playlist name"
            />
            <datalist id="playlists">
              {playlists.map((p) => (
                <option key={p.id} value={p.name} />
              ))}
            </datalist>
            <button onClick={push} disabled={!!busy || readyToPush === 0 || !playlistName.trim()}>
              Add {readyToPush} to playlist
            </button>
          </div>
        </>
      )}
    </div>
  );
}

function RowView({
  row,
  checked,
  chosenUri,
  onToggle,
  onChoose,
}: {
  row: MatchRow;
  checked: boolean;
  chosenUri?: string;
  onToggle: () => void;
  onChoose: (uri: string) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const best = row.candidates.find((c) => c.track.uri === chosenUri) ?? row.candidates[0];
  const alternatives = row.candidates.length > 1;

  return (
    <>
      <tr className={row.verdict}>
        <td className="tick">
          <input type="checkbox" checked={checked} onChange={onToggle} disabled={!chosenUri} />
        </td>
        <td>
          <div>{row.track.artist || <span className="dim">untagged</span>}</div>
          <div className="dim small">{row.track.parsed.base}</div>
        </td>
        <td className={row.track.parsed.kind === "None" ? "dim" : "version"}>
          {descriptorLabel(row.track)}
          <div className="dim small">{duration(row.track.duration_ms)}</div>
        </td>
        <td>
          {best ? (
            <>
              <div>{best.track.name}</div>
              <div className="dim small">
                {best.track.artists.join(", ")} · {duration(best.track.duration_ms)}
                {best.score.duration_delta_ms > 20000 && (
                  <span className="warn"> · {Math.round(best.score.duration_delta_ms / 1000)}s shorter</span>
                )}
              </div>
            </>
          ) : (
            <span className="dim">{row.cached ? "cached — re-check to see options" : row.reason}</span>
          )}
          {alternatives && (
            <button className="link" onClick={() => setExpanded((v) => !v)}>
              {expanded ? "hide" : `${row.candidates.length - 1} other option${row.candidates.length > 2 ? "s" : ""}`}
            </button>
          )}
        </td>
        <td className="num">
          <span className={`badge ${row.verdict}`}>{Math.round(row.confidence * 100)}%</span>
        </td>
      </tr>
      {expanded &&
        row.candidates.map((c: Candidate) => (
          <tr key={c.track.uri} className="alt">
            <td></td>
            <td colSpan={4}>
              <label>
                <input
                  type="radio"
                  name={`c-${row.track_id}`}
                  checked={c.track.uri === chosenUri}
                  onChange={() => onChoose(c.track.uri)}
                />{" "}
                {c.track.name} <span className="dim">— {c.track.artists.join(", ")} · {duration(c.track.duration_ms)} · {Math.round(c.score.total * 100)}%</span>
              </label>
            </td>
          </tr>
        ))}
    </>
  );
}

function Stat({ label, value, tone }: { label: string; value: string | number; tone?: "good" | "muted" }) {
  return (
    <div className={`stat ${tone ?? ""}`}>
      <span className="value">{value}</span>
      <span className="label">{label}</span>
    </div>
  );
}
