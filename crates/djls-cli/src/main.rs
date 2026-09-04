//! Headless matcher — Phase 0.5.
//!
//! The point of this binary is to answer one question before any UI exists:
//! what percentage of a real download folder actually resolves on Spotify?
//! Run it against your own library and let the number decide the roadmap.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use djls_core::matcher::{evaluate, MatchOutcome, Thresholds};
use djls_core::normalize::parse_title;
use djls_core::tags::{scan_folder, LocalTrack};
use djls_core::watcher::{watch_folder, WatcherConfig};
use djls_core::{MatchMethod, SpotifyClient, Verdict};

/// Candidates requested per query. Wider than Spotify's ranking needs for a
/// clean hit so that an extended mix buried behind a radio edit still shows
/// up in the results the matcher gets to score.
const SEARCH_LIMIT: u32 = 20;

#[derive(Parser)]
#[command(name = "djls", about = "DJ Library Sync — headless matcher and folder watcher")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read tags from a folder and show how each title parses. No network.
    Scan {
        folder: PathBuf,
        #[arg(long)]
        no_recursive: bool,
    },
    /// Match a folder against Spotify and report the hit rate.
    Match {
        folder: PathBuf,
        #[arg(long)]
        no_recursive: bool,
        /// Stop after N tracks — useful for a quick read on a huge folder.
        #[arg(long)]
        limit: Option<usize>,
        /// Restrict results to a market (e.g. GB, US) so the rate reflects
        /// what is actually available to you.
        #[arg(long)]
        market: Option<String>,
        /// Write a per-track CSV report here.
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Print every track as it is processed.
        #[arg(long)]
        verbose: bool,
    },
    /// Watch a folder and print tracks as they finish downloading.
    Watch {
        folder: PathBuf,
        /// Also emit files already present when watching starts.
        #[arg(long)]
        include_existing: bool,
    },
    /// Parse a single title string. Handy for checking the mix-descriptor logic.
    Parse { title: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    match cli.command {
        Command::Scan { folder, no_recursive } => cmd_scan(&folder, !no_recursive),
        Command::Match {
            folder,
            no_recursive,
            limit,
            market,
            csv,
            verbose,
        } => cmd_match(&folder, !no_recursive, limit, market, csv, verbose).await,
        Command::Watch { folder, include_existing } => cmd_watch(&folder, include_existing),
        Command::Parse { title } => {
            let p = parse_title(&title);
            println!("raw        {}", p.raw);
            println!("base       {}", p.base);
            println!("kind       {:?}", p.kind);
            println!("remixer    {}", p.remixer.as_deref().unwrap_or("-"));
            println!("featured   {}", if p.featured.is_empty() { "-".to_string() } else { p.featured.join(", ") });
            println!("descriptor {}", p.descriptor_label());
            Ok(())
        }
    }
}

fn load_tracks(folder: &Path, recursive: bool) -> Result<Vec<LocalTrack>> {
    if !folder.is_dir() {
        bail!("{} is not a folder", folder.display());
    }
    let (tracks, failures) = scan_folder(folder, recursive);

    if !failures.is_empty() {
        eprintln!("\n{} file(s) could not be read:", failures.len());
        for (path, err) in failures.iter().take(10) {
            eprintln!("  {}: {}", path.display(), err);
        }
        if failures.len() > 10 {
            eprintln!("  ... and {} more", failures.len() - 10);
        }
        eprintln!();
    }

    Ok(tracks)
}

fn cmd_scan(folder: &Path, recursive: bool) -> Result<()> {
    let tracks = load_tracks(folder, recursive)?;
    if tracks.is_empty() {
        println!("No audio files found in {}", folder.display());
        return Ok(());
    }

    println!(
        "{:<34} {:<26} {:<16} {:>7} {:>6}",
        "ARTIST", "TITLE (BASE)", "VERSION", "LENGTH", "ISRC"
    );
    println!("{}", "-".repeat(94));

    let mut with_isrc = 0usize;
    let mut with_version = 0usize;

    for track in &tracks {
        if track.isrc.is_some() {
            with_isrc += 1;
        }
        if track.parsed.kind != djls_core::MixKind::None {
            with_version += 1;
        }
        println!(
            "{:<34} {:<26} {:<16} {:>7} {:>6}",
            truncate(&track.artist, 33),
            truncate(&track.parsed.base, 25),
            truncate(&track.parsed.descriptor_label(), 15),
            track.duration_display(),
            if track.isrc.is_some() { "yes" } else { "-" }
        );
    }

    let total = tracks.len();
    println!("\n{total} track(s)");
    println!(
        "  ISRC in tags        {with_isrc:>4}  ({:.0}%)  <- the high-confidence match path",
        pct(with_isrc, total)
    );
    println!(
        "  version descriptor  {with_version:>4}  ({:.0}%)",
        pct(with_version, total)
    );
    Ok(())
}

async fn cmd_match(
    folder: &Path,
    recursive: bool,
    limit: Option<usize>,
    market: Option<String>,
    csv_path: Option<PathBuf>,
    verbose: bool,
) -> Result<()> {
    let client_id = std::env::var("SPOTIFY_CLIENT_ID").ok().filter(|s| !s.is_empty());
    let client_secret = std::env::var("SPOTIFY_CLIENT_SECRET").ok().filter(|s| !s.is_empty());

    let (Some(client_id), Some(client_secret)) = (client_id, client_secret) else {
        bail!(
            "Set SPOTIFY_CLIENT_ID and SPOTIFY_CLIENT_SECRET (copy .env.example to .env).\n\
             Create an app at https://developer.spotify.com/dashboard — no redirect URI needed \
             for this command, it uses the Client Credentials flow."
        );
    };

    let client = SpotifyClient::new(client_id, client_secret)?.with_market(market);

    let mut tracks = load_tracks(folder, recursive)?;
    if let Some(limit) = limit {
        tracks.truncate(limit);
    }
    if tracks.is_empty() {
        println!("No audio files found in {}", folder.display());
        return Ok(());
    }

    let thresholds = Thresholds::default();
    let total = tracks.len();
    let mut rows: Vec<(LocalTrack, MatchOutcome)> = Vec::with_capacity(total);

    for (index, track) in tracks.into_iter().enumerate() {
        eprint!("\rMatching {}/{}...", index + 1, total);

        let isrc_hits = match &track.isrc {
            Some(isrc) => client.search_isrc(isrc).await.unwrap_or_default(),
            None => Vec::new(),
        };

        let text_hits = if isrc_hits.is_empty() {
            client.search_for_track(&track, SEARCH_LIMIT).await.unwrap_or_default()
        } else {
            Vec::new()
        };

        let outcome = evaluate(&track, &isrc_hits, &text_hits, thresholds);

        if verbose {
            eprintln!();
            print_track(&track, &outcome);
        }

        rows.push((track, outcome));
    }
    eprintln!("\r{}", " ".repeat(30));

    print_summary(&rows, &client);

    if let Some(path) = csv_path {
        write_csv(&path, &rows).with_context(|| format!("writing {}", path.display()))?;
        println!("\nPer-track report written to {}", path.display());
    } else {
        println!("\nRe-run with --csv report.csv for the per-track breakdown.");
    }

    Ok(())
}

fn print_track(track: &LocalTrack, outcome: &MatchOutcome) {
    let marker = match outcome.verdict {
        Verdict::Auto => "OK  ",
        Verdict::Review => "?   ",
        Verdict::NoMatch => "MISS",
    };
    println!(
        "{marker} {} - {} [{}] {}",
        truncate(&track.artist, 28),
        truncate(&track.parsed.base, 30),
        track.parsed.descriptor_label(),
        track.duration_display()
    );
    match outcome.best() {
        Some(best) => println!(
            "       -> {} - {} [{}] {}  conf {:.0}% via {} ({}s delta)",
            truncate(&best.track.artist_field(), 28),
            truncate(&best.track.name, 34),
            best.track.duration_display(),
            best.track.url.as_deref().unwrap_or("-"),
            best.score.total * 100.0,
            outcome.method.as_str(),
            best.score.duration_delta_ms / 1000
        ),
        None => println!("       -> nothing returned ({})", outcome.reason),
    }
}

fn print_summary(rows: &[(LocalTrack, MatchOutcome)], client: &SpotifyClient) {
    let total = rows.len();
    let mut by_verdict: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_method: BTreeMap<&str, usize> = BTreeMap::new();
    let mut miss_reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut isrc_tagged = 0usize;
    let mut isrc_resolved = 0usize;

    for (track, outcome) in rows {
        *by_verdict.entry(outcome.verdict.as_str()).or_default() += 1;
        *by_method.entry(outcome.method.as_str()).or_default() += 1;

        if track.isrc.is_some() {
            isrc_tagged += 1;
            if outcome.method == MatchMethod::Isrc {
                isrc_resolved += 1;
            }
        }
        if outcome.verdict == Verdict::NoMatch {
            *miss_reasons.entry(outcome.reason.clone()).or_default() += 1;
        }
    }

    let auto = *by_verdict.get("auto").unwrap_or(&0);
    let review = *by_verdict.get("review").unwrap_or(&0);
    let miss = *by_verdict.get("no_match").unwrap_or(&0);

    println!("\n=== Match rate over {total} track(s) ===\n");
    println!("  auto-push      {auto:>4}   {:>5.1}%", pct(auto, total));
    println!("  needs review   {review:>4}   {:>5.1}%", pct(review, total));
    println!("  no match       {miss:>4}   {:>5.1}%", pct(miss, total));

    println!("\n  matched via:");
    for (method, count) in &by_method {
        if *method == "none" {
            continue;
        }
        println!("    {method:<8} {count:>4}   {:>5.1}%", pct(*count, total));
    }

    println!(
        "\n  ISRC present in tags: {isrc_tagged}/{total} ({:.0}%), resolved on Spotify: {isrc_resolved}",
        pct(isrc_tagged, total)
    );

    if !miss_reasons.is_empty() {
        println!("\n  why tracks missed:");
        let mut reasons: Vec<_> = miss_reasons.into_iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(&a.1));
        for (reason, count) in reasons.into_iter().take(6) {
            println!("    {count:>4}  {}", truncate(&reason, 68));
        }
    }

    println!(
        "\n  API: {} request(s), {} rate-limit wait(s), {} error(s)",
        client.stats.requests.load(Ordering::Relaxed),
        client.stats.rate_limited.load(Ordering::Relaxed),
        client.stats.errors.load(Ordering::Relaxed)
    );
}

fn write_csv(path: &Path, rows: &[(LocalTrack, MatchOutcome)]) -> Result<()> {
    let mut writer = csv::Writer::from_path(path)?;
    writer.write_record([
        "file",
        "local_artist",
        "local_title_base",
        "local_version",
        "local_length",
        "local_isrc",
        "verdict",
        "method",
        "confidence",
        "spotify_artist",
        "spotify_title",
        "spotify_length",
        "delta_seconds",
        "spotify_url",
        "spotify_uri",
        "notes",
    ])?;

    for (track, outcome) in rows {
        let best = outcome.best();
        writer.write_record([
            track.file_name.clone(),
            track.artist.clone(),
            track.parsed.base.clone(),
            track.parsed.descriptor_label(),
            track.duration_display(),
            track.isrc.clone().unwrap_or_default(),
            outcome.verdict.as_str().to_string(),
            outcome.method.as_str().to_string(),
            format!("{:.0}", outcome.confidence() * 100.0),
            best.map(|c| c.track.artist_field()).unwrap_or_default(),
            best.map(|c| c.track.name.clone()).unwrap_or_default(),
            best.map(|c| c.track.duration_display()).unwrap_or_default(),
            best.map(|c| (c.score.duration_delta_ms / 1000).to_string())
                .unwrap_or_default(),
            best.and_then(|c| c.track.url.clone()).unwrap_or_default(),
            best.map(|c| c.track.uri.clone()).unwrap_or_default(),
            best.map(|c| c.score.notes.join("; "))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| outcome.reason.clone()),
        ])?;
    }

    writer.flush()?;
    Ok(())
}

fn cmd_watch(folder: &Path, include_existing: bool) -> Result<()> {
    if !folder.is_dir() {
        bail!("{} is not a folder", folder.display());
    }

    println!("Watching {} — drop a file in to test. Ctrl-C to stop.\n", folder.display());

    let config = WatcherConfig {
        emit_existing: include_existing,
        ..Default::default()
    };

    let _watcher = watch_folder(folder, config, |path| match LocalTrack::read(&path) {
        Ok(track) => println!(
            "ready  {} - {} [{}] {} isrc={}",
            track.artist,
            track.parsed.base,
            track.parsed.descriptor_label(),
            track.duration_display(),
            track.isrc.as_deref().unwrap_or("-")
        ),
        Err(err) => println!("ready  {} (tags unreadable: {err:#})", path.display()),
    })?;

    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

fn pct(part: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}

fn truncate(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        input.to_string()
    } else {
        let head: String = input.chars().take(max.saturating_sub(1)).collect();
        format!("{head}\u{2026}")
    }
}
