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
use djls_core::auth::{self, AuthConfig, KeyringStore, TokenStore};
use djls_core::db::{Database, PLATFORM_SPOTIFY};
use djls_core::matcher::{evaluate, MatchOutcome, ShorterVersionPolicy, Thresholds};
use djls_core::normalize::parse_title;
use djls_core::tags::{scan_folder, LocalTrack};
use djls_core::watcher::{watch_folder, WatcherConfig};
use djls_core::{MatchMethod, SpotifyClient, Verdict};

/// Candidates requested per query. Spotify caps this at 10 for a development
/// -mode app, so the candidate pool is widened with extra queries instead —
/// see `SpotifyClient::search_for_track`.
const SEARCH_LIMIT: u32 = 10;

/// Stop once it is clear the API is failing systemically rather than a query
/// here and there. A report built on failed searches understates the match
/// rate instead of reporting an error, which is worse than no report at all.
const MIN_ATTEMPTS_BEFORE_ABORT: usize = 5;
const ABORT_ERROR_RATE: f64 = 0.5;

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
        /// Push the shorter version when the extended mix isn't on Spotify,
        /// instead of parking it for review.
        #[arg(long, conflicts_with = "reject_shorter")]
        accept_shorter: bool,
        /// Treat a song that is only available as a shorter cut as no match.
        #[arg(long)]
        reject_shorter: bool,
        /// Re-query Spotify even for files already matched in a previous run.
        #[arg(long)]
        rescan: bool,
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
    /// Connect your Spotify account (opens a browser).
    Login {
        /// Print the authorization URL instead of opening a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Forget the stored Spotify tokens.
    Logout,
    /// Show which Spotify account is connected.
    Whoami,
    /// List the playlists on the connected account.
    Playlists,
    /// Match a folder and add the confident matches to a playlist.
    Push {
        folder: PathBuf,
        /// Target playlist by name. Created if it does not exist; defaults to
        /// "New Downloads <today>".
        #[arg(long)]
        playlist: Option<String>,
        #[arg(long)]
        no_recursive: bool,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        market: Option<String>,
        /// Push the shorter version when the extended mix isn't on Spotify.
        #[arg(long)]
        accept_shorter: bool,
        /// Work out what would be added, then stop without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Re-query Spotify even for files already matched in a previous run.
        #[arg(long)]
        rescan: bool,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// List tracks nothing could be found for — the AcoustID queue.
    Misses,
    /// Show what the local database has recorded.
    Stats,
}

/// Tokens live in the OS credential store, never in a file on disk.
fn token_store() -> Box<dyn TokenStore> {
    Box::new(KeyringStore::default())
}

/// PKCE needs only the client ID — there is no secret in this flow.
fn auth_config() -> Result<AuthConfig> {
    let client_id = std::env::var("SPOTIFY_CLIENT_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("Set SPOTIFY_CLIENT_ID (copy .env.example to .env)")
        })?;
    Ok(AuthConfig::new(client_id))
}

fn user_client(market: Option<String>) -> Result<SpotifyClient> {
    Ok(SpotifyClient::for_user(auth_config()?, token_store())?.with_market(market))
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
            accept_shorter,
            reject_shorter,
            rescan,
            verbose,
        } => {
            let shorter_version = if accept_shorter {
                ShorterVersionPolicy::Accept
            } else if reject_shorter {
                ShorterVersionPolicy::Reject
            } else {
                ShorterVersionPolicy::Review
            };
            cmd_match(
                &folder,
                !no_recursive,
                limit,
                market,
                csv,
                shorter_version,
                rescan,
                verbose,
            )
            .await
        }
        Command::Watch { folder, include_existing } => cmd_watch(&folder, include_existing),
        Command::Login { no_browser } => cmd_login(no_browser).await,
        Command::Logout => {
            token_store().clear()?;
            println!("Signed out — stored tokens removed.");
            Ok(())
        }
        Command::Whoami => {
            let me = user_client(None)?.current_user().await?;
            println!(
                "{} ({})",
                me.display_name.as_deref().unwrap_or("(no display name)"),
                me.id
            );
            Ok(())
        }
        Command::Playlists => cmd_playlists().await,
        Command::Misses => cmd_misses(),
        Command::Stats => cmd_stats(),
        Command::Push {
            folder,
            playlist,
            no_recursive,
            limit,
            market,
            accept_shorter,
            dry_run,
            rescan,
            yes,
        } => {
            cmd_push(
                &folder,
                playlist,
                !no_recursive,
                limit,
                market,
                if accept_shorter {
                    ShorterVersionPolicy::Accept
                } else {
                    ShorterVersionPolicy::Review
                },
                dry_run,
                rescan,
                yes,
            )
            .await
        }
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
    shorter_version: ShorterVersionPolicy,
    rescan: bool,
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

    let thresholds = Thresholds {
        shorter_version,
        ..Thresholds::default()
    };
    let db = Database::open(&Database::default_path())?;
    let total = tracks.len();
    let mut rows: Vec<Row> = Vec::with_capacity(total);
    let mut failed = 0usize;
    let mut reused = 0usize;

    for (index, track) in tracks.into_iter().enumerate() {
        eprint!("\rMatching {}/{}...", index + 1, total);

        let record = db.upsert_track(&track)?;

        // A file already resolved in a previous run costs nothing to skip,
        // and skipping is the whole point of keeping state.
        if !rescan && record.can_reuse_match() {
            if let Some(stored) = db.stored_match(record.id, PLATFORM_SPOTIFY)? {
                reused += 1;
                rows.push(Row {
                    track,
                    outcome: MatchOutcome {
                        verdict: stored.verdict,
                        method: MatchMethod::None,
                        candidates: Vec::new(),
                        reason: format!("{} (from cache)", stored.reason),
                    },
                    search_error: None,
                });
                continue;
            }
        }

        // A failed search is not the same thing as a track that isn't on
        // Spotify. Keep the distinction — conflating them turns an outage
        // into a confidently wrong match rate.
        let mut search_error = None;

        let isrc_hits = match &track.isrc {
            Some(isrc) => match client.search_isrc(isrc).await {
                Ok(hits) => hits,
                Err(err) => {
                    search_error = Some(format!("{err:#}"));
                    Vec::new()
                }
            },
            None => Vec::new(),
        };

        let text_hits = if isrc_hits.is_empty() {
            match client.search_for_track(&track, SEARCH_LIMIT).await {
                Ok(hits) => hits,
                Err(err) => {
                    search_error = Some(format!("{err:#}"));
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        if search_error.is_some() {
            failed += 1;
        }

        let outcome = evaluate(&track, &isrc_hits, &text_hits, thresholds);

        if verbose {
            eprintln!();
            print_track(&track, &outcome);
        }

        let attempts = index + 1;
        if attempts >= MIN_ATTEMPTS_BEFORE_ABORT
            && (failed as f64 / attempts as f64) >= ABORT_ERROR_RATE
        {
            eprintln!("\r{}", " ".repeat(30));
            bail!(
                "Aborting: {failed} of the first {attempts} searches failed, so any match rate \
                 from this run would be meaningless.\n\nLast error: {}",
                search_error
                    .as_deref()
                    .or(rows.iter().rev().find_map(|r| r.search_error.as_deref()))
                    .unwrap_or("unknown")
            );
        }

        // Only record a real answer; a failed search must not be cached as
        // though Spotify had said "not found".
        if search_error.is_none() {
            db.record_match(record.id, PLATFORM_SPOTIFY, &outcome)?;
        }

        rows.push(Row {
            track,
            outcome,
            search_error,
        });
    }
    eprintln!("\r{}", ".".repeat(0));

    if reused > 0 {
        println!("{reused} track(s) reused from a previous run — pass --rescan to re-query.\n");
    }

    print_summary(&rows, &client);

    if let Some(path) = csv_path {
        write_csv(&path, &rows).with_context(|| format!("writing {}", path.display()))?;
        println!("\nPer-track report written to {}", path.display());
    } else {
        println!("\nRe-run with --csv report.csv for the per-track breakdown.");
    }

    Ok(())
}

/// One track's result. `search_error` is kept separate from the verdict so a
/// failed lookup is never counted as "not on Spotify".
struct Row {
    track: LocalTrack,
    outcome: MatchOutcome,
    search_error: Option<String>,
}


async fn cmd_login(no_browser: bool) -> Result<()> {
    let config = auth_config()?;
    let store = token_store();

    println!("Redirect URI: {}", config.redirect_uri());
    println!(
        "This exact URI must be listed under \"Redirect URIs\" on your app at\n\
         https://developer.spotify.com/dashboard — Spotify rejects `localhost`,\n\
         it has to be the 127.0.0.1 form.\n"
    );

    auth::login(&config, store.as_ref(), |url| {
        if !no_browser && auth::open_in_browser(url) {
            println!("Opened your browser to approve access.");
        } else {
            println!("Open this URL to approve access:");
        }
        println!("\n{url}\n");
    })
    .await?;

    let me = SpotifyClient::for_user(auth_config()?, token_store())?
        .current_user()
        .await?;
    println!(
        "Signed in as {} ({}). Tokens stored in your OS keychain.",
        me.display_name.as_deref().unwrap_or("(no display name)"),
        me.id
    );
    Ok(())
}

async fn cmd_playlists() -> Result<()> {
    let client = user_client(None)?;
    let me = client.current_user().await?;
    let playlists = client.list_playlists().await?;

    if playlists.is_empty() {
        println!("No playlists on this account yet.");
        return Ok(());
    }

    println!("{:<44} {:>7}  {}", "NAME", "TRACKS", "OWNER");
    println!("{}", "-".repeat(70));
    for p in &playlists {
        println!(
            "{:<44} {:>7}  {}",
            truncate(&p.name, 43),
            p.track_count()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string()),
            if p.is_owned_by(&me.id) { "you" } else { "-" }
        );
    }
    println!("\n{} playlist(s)", playlists.len());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_push(
    folder: &Path,
    playlist_name: Option<String>,
    recursive: bool,
    limit: Option<usize>,
    market: Option<String>,
    shorter_version: ShorterVersionPolicy,
    dry_run: bool,
    rescan: bool,
    assume_yes: bool,
) -> Result<()> {
    let client = user_client(market)?;
    let me = client.current_user().await?;

    let mut tracks = load_tracks(folder, recursive)?;
    if let Some(limit) = limit {
        tracks.truncate(limit);
    }
    if tracks.is_empty() {
        println!("No audio files found in {}", folder.display());
        return Ok(());
    }

    let thresholds = Thresholds {
        shorter_version,
        ..Thresholds::default()
    };

    let db = Database::open(&Database::default_path())?;
    let total = tracks.len();
    let mut pushable: Vec<(LocalTrack, MatchOutcome, i64)> = Vec::new();
    let mut review = 0usize;
    let mut missing = 0usize;
    let mut failed = 0usize;
    let mut reused = 0usize;

    for (index, track) in tracks.into_iter().enumerate() {
        eprint!("\rMatching {}/{}...", index + 1, total);

        let record = db.upsert_track(&track)?;

        if !rescan && record.can_reuse_match() {
            if let Some(stored) = db.stored_match(record.id, PLATFORM_SPOTIFY)? {
                reused += 1;
                match (stored.verdict, stored.platform_uri.is_some()) {
                    (Verdict::Auto, true) => {
                        pushable.push((track, cached_outcome(&stored), record.id))
                    }
                    (Verdict::Auto, false) | (Verdict::Review, _) => review += 1,
                    (Verdict::NoMatch, _) => missing += 1,
                }
                continue;
            }
        }

        let isrc_hits = match &track.isrc {
            Some(isrc) => client.search_isrc(isrc).await.unwrap_or_default(),
            None => Vec::new(),
        };
        let text_hits = if isrc_hits.is_empty() {
            match client.search_for_track(&track, SEARCH_LIMIT).await {
                Ok(hits) => hits,
                Err(_) => {
                    failed += 1;
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        let outcome = evaluate(&track, &isrc_hits, &text_hits, thresholds);
        db.record_match(record.id, PLATFORM_SPOTIFY, &outcome)?;

        match outcome.verdict {
            // Only confident matches are pushed. Everything ambiguous waits for
            // a human — that separation is the whole point of the verdicts.
            Verdict::Auto => pushable.push((track, outcome, record.id)),
            Verdict::Review => review += 1,
            Verdict::NoMatch => missing += 1,
        }
    }
    eprintln!("\r{}", " ".repeat(30));

    if reused > 0 {
        println!("{reused} track(s) reused from a previous run — pass --rescan to re-query.");
    }

    if failed > 0 {
        println!("!! {failed} search(es) failed and were skipped.\n");
    }

    println!(
        "{} confident match(es); {review} need review, {missing} not found.",
        pushable.len()
    );

    if pushable.is_empty() {
        println!("Nothing to push.");
        return Ok(());
    }

    let name = playlist_name.unwrap_or_else(default_playlist_name);
    let existing = client
        .list_playlists()
        .await?
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(&name) && p.is_owned_by(&me.id));

    // Re-running must not stack duplicates; Spotify will happily add the same
    // track twice if asked.
    let already = match &existing {
        Some(p) => client.playlist_entries(&p.id).await?,
        None => Vec::new(),
    };

    let mut to_add: Vec<(String, &LocalTrack, i64)> = Vec::new();
    let mut skipped = 0usize;
    for (track, outcome, track_id) in &pushable {
        let Some(best) = outcome.best() else { continue };

        // Three guards: what the playlist currently holds, what we have logged
        // pushing before, and duplicates within this batch itself. The first
        // compares recordings rather than URIs, because Spotify indexes the
        // same recording under several of them.
        let in_playlist = already.iter().any(|e| e.is_same_recording_as(&best.track));

        let logged = existing
            .as_ref()
            .map(|p| db.already_synced(*track_id, &p.id, PLATFORM_SPOTIFY))
            .transpose()?
            .unwrap_or(false);
        let in_batch = to_add.iter().any(|(uri, _, _)| uri == &best.track.uri);

        if in_playlist || logged || in_batch {
            skipped += 1;
            continue;
        }
        to_add.push((best.track.uri.clone(), track, *track_id));
    }

    if skipped > 0 {
        println!("{skipped} already in the playlist — skipping those.");
    }
    if to_add.is_empty() {
        println!("Everything is already in \"{name}\". Nothing to do.");
        return Ok(());
    }

    println!(
        "\nWould add {} track(s) to \"{name}\"{}:",
        to_add.len(),
        if existing.is_some() { "" } else { " (new playlist)" }
    );
    for (_, track, _) in to_add.iter().take(10) {
        println!("  {} - {}", track.artist, track.title);
    }
    if to_add.len() > 10 {
        println!("  ... and {} more", to_add.len() - 10);
    }

    if dry_run {
        println!("\nDry run — nothing was written.");
        return Ok(());
    }

    if !assume_yes && !confirm("\nAdd these to your Spotify account?")? {
        println!("Cancelled.");
        return Ok(());
    }

    let playlist = match existing {
        Some(p) => p,
        None => client.create_playlist(&name, false).await?,
    };

    let uris: Vec<String> = to_add.iter().map(|(uri, _, _)| uri.clone()).collect();
    let added = client.add_tracks_to_playlist(&playlist.id, &uris).await?;

    // Logged only after the write succeeds, so a failed push is retried rather
    // than silently recorded as done.
    for (uri, _, track_id) in &to_add {
        db.record_sync(*track_id, PLATFORM_SPOTIFY, uri, &playlist.id)?;
    }

    println!("Added {added} track(s) to \"{}\".", playlist.name);
    Ok(())
}

/// Rebuild an outcome from a cached match, so a reused row can flow through
/// the same code path as a freshly-matched one — including the duplicate
/// check, which needs real metadata rather than a bare URI.
fn cached_outcome(stored: &djls_core::db::StoredMatch) -> MatchOutcome {
    let uri = stored.platform_uri.clone().unwrap_or_default();
    MatchOutcome {
        verdict: Verdict::Auto,
        method: MatchMethod::None,
        candidates: vec![djls_core::Candidate {
            track: djls_core::SpotifyTrack {
                id: uri.rsplit(':').next().unwrap_or_default().to_string(),
                uri: uri.clone(),
                name: stored.platform_name.clone().unwrap_or_default(),
                artists: stored
                    .platform_artists
                    .as_deref()
                    .map(|a| a.split(", ").map(|s| s.to_string()).collect())
                    .unwrap_or_default(),
                album: String::new(),
                duration_ms: stored.platform_duration_ms.unwrap_or(0),
                isrc: None,
                url: None,
                popularity: None,
            },
            score: djls_core::Score {
                total: 1.0,
                artist: 1.0,
                title: 1.0,
                duration: 1.0,
                mix: 1.0,
                duration_delta_ms: 0,
                notes: Vec::new(),
            },
        }],
        reason: format!("{} (from cache)", stored.reason),
    }
}

fn cmd_misses() -> Result<()> {
    let db = Database::open(&Database::default_path())?;
    let missed = db.missed_tracks(PLATFORM_SPOTIFY)?;

    if missed.is_empty() {
        println!("Nothing in the no-match queue.");
        return Ok(());
    }

    println!("{} track(s) with no match on Spotify:\n", missed.len());
    for m in &missed {
        println!("  {} - {}", truncate(&m.artist, 32), truncate(&m.title, 40));
        println!("    {}", truncate(&m.reason, 68));
    }
    println!("\nThese are the candidates for audio fingerprinting later.");
    Ok(())
}

fn cmd_stats() -> Result<()> {
    let path = Database::default_path();
    let db = Database::open(&path)?;
    let (tracks, matches, synced) = db.counts()?;

    println!("database   {}", path.display());
    println!("tracks     {tracks}");
    println!("matches    {matches}");
    println!("pushed     {synced}");
    Ok(())
}

/// Named for the *local* date. Computing this in UTC means a push after
/// ~18:00 in the Americas gets tomorrow's date on the playlist.
fn default_playlist_name() -> String {
    format!("New Downloads {}", chrono::Local::now().format("%Y-%m-%d"))
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write;
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
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

fn print_summary(rows: &[Row], client: &SpotifyClient) {
    let mut by_verdict: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_method: BTreeMap<&str, usize> = BTreeMap::new();
    let mut miss_reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut search_errors: BTreeMap<String, usize> = BTreeMap::new();
    let mut isrc_tagged = 0usize;
    let mut isrc_resolved = 0usize;

    for row in rows {
        // Rows whose search failed say nothing about availability, so they are
        // reported on their own rather than folded into the rate.
        if let Some(err) = &row.search_error {
            *search_errors.entry(err.clone()).or_default() += 1;
            continue;
        }

        *by_verdict.entry(row.outcome.verdict.as_str()).or_default() += 1;
        *by_method.entry(row.outcome.method.as_str()).or_default() += 1;

        if row.track.isrc.is_some() {
            isrc_tagged += 1;
            if row.outcome.method == MatchMethod::Isrc {
                isrc_resolved += 1;
            }
        }
        if row.outcome.verdict == Verdict::NoMatch {
            *miss_reasons.entry(row.outcome.reason.clone()).or_default() += 1;
        }
    }

    let failed: usize = search_errors.values().sum();
    let total = rows.len() - failed;

    let auto = *by_verdict.get("auto").unwrap_or(&0);
    let review = *by_verdict.get("review").unwrap_or(&0);
    let miss = *by_verdict.get("no_match").unwrap_or(&0);

    if failed > 0 {
        println!(
            "\n!! {failed} of {} track(s) could not be searched — they are excluded below,\n\
             !! so this rate covers only the {total} track(s) that got a real answer.",
            rows.len()
        );
        for (err, count) in &search_errors {
            println!("   {count:>4}  {}", truncate(err, 68));
        }
    }

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

fn write_csv(path: &Path, rows: &[Row]) -> Result<()> {
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
        "score_artist",
        "score_title",
        "score_duration",
        "score_mix",
        "runner_up",
        "search_error",
        "notes",
    ])?;

    for Row {
        track,
        outcome,
        search_error,
    } in rows
    {
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
            best.map(|c| format!("{:.2}", c.score.artist)).unwrap_or_default(),
            best.map(|c| format!("{:.2}", c.score.title)).unwrap_or_default(),
            best.map(|c| format!("{:.2}", c.score.duration)).unwrap_or_default(),
            best.map(|c| format!("{:.2}", c.score.mix)).unwrap_or_default(),
            outcome
                .candidates
                .get(1)
                .map(|c| {
                    format!(
                        "{} - {} [{}] ({:.0}%)",
                        c.track.artist_field(),
                        c.track.name,
                        c.track.duration_display(),
                        c.score.total * 100.0
                    )
                })
                .unwrap_or_default(),
            search_error.clone().unwrap_or_default(),
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
