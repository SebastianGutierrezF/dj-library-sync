export type MixKind =
  | "None"
  | "Original"
  | "Extended"
  | "Radio"
  | "Club"
  | "Dub"
  | "Instrumental"
  | "Acapella"
  | "Edit"
  | "Vip"
  | "Remix"
  | "Rework";

export interface ParsedTitle {
  raw: string;
  base: string;
  base_norm: string;
  kind: MixKind;
  remixer: string | null;
  remixer_norm: string | null;
  featured: string[];
  mix_raw: string | null;
}

export interface LocalTrack {
  path: string;
  file_name: string;
  artist: string;
  title: string;
  album: string | null;
  isrc: string | null;
  bpm: string | null;
  musical_key: string | null;
  duration_ms: number;
  file_size: number;
  parsed: ParsedTitle;
}

export interface ScanResult {
  tracks: LocalTrack[];
  failures: { path: string; error: string }[];
}

export interface DetectedFile {
  path: string;
  track: LocalTrack | null;
  error: string | null;
}

export interface SpotifyTrack {
  id: string;
  uri: string;
  name: string;
  artists: string[];
  album: string;
  duration_ms: number;
  isrc: string | null;
  url: string | null;
  popularity: number | null;
}

export interface Score {
  total: number;
  artist: number;
  title: number;
  duration: number;
  mix: number;
  duration_delta_ms: number;
  notes: string[];
}

export interface Candidate {
  track: SpotifyTrack;
  score: Score;
}

export type Verdict = "auto" | "review" | "no_match";

export interface MatchRow {
  track_id: number;
  track: LocalTrack;
  verdict: Verdict;
  method: string;
  confidence: number;
  reason: string;
  candidates: Candidate[];
  cached: boolean;
}

export interface AppConfig {
  spotify_client_id: string | null;
  watch_folder: string | null;
  accept_shorter: boolean;
  last_playlist: string | null;
}

export interface AccountStatus {
  configured: boolean;
  signed_in: boolean;
  display_name: string | null;
  user_id: string | null;
  error: string | null;
}

export interface PlaylistInfo {
  id: string;
  name: string;
  track_count: number | null;
  owned: boolean;
}

export interface PushResult {
  playlist_name: string;
  added: number;
  skipped: number;
}

export interface PlatformOption {
  id: string;
  display_name: string;
  credentials: "user_provided" | "hosted";
  metered: boolean;
  available: boolean;
  connected: boolean;
}
