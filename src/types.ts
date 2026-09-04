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
