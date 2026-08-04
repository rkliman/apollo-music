use config as app_config;
use lofty::file::TaggedFileExt;
use lofty::prelude::ItemKey;
use lofty::file::AudioFile;
use clap::{Parser, Subcommand, ArgAction};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::fs;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::HashMap;
use globset::{Glob, GlobSetBuilder};
use rayon::prelude::*;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::thread;
use lofty::config::WriteOptions;
use lofty::tag::TagExt; // needed for insert_text / save_to_path — remove if already in scope

use deunicode::deunicode;

fn normalize_text(s: &str) -> String {
    deunicode(s)
}

const CONFIG_PATH: &str = "~/.config/apollo-music/config.toml";

// ---------------------------------------------------------------------
// Helper functions to replace removed dependencies
// ---------------------------------------------------------------------

fn expand_tilde(path: &str) -> String {
    if path.starts_with('~') {
        if let Some(home) = std::env::var_os("HOME") {
            return path.replacen('~', &home.to_string_lossy(), 1);
        }
    }
    path.to_string()
}

fn format_bytes(bytes: f64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024.0 {
        return format!("{:.2} B", bytes);
    }
    let exp = (bytes.ln() / 1024_f64.ln()).floor() as usize;
    let exp = exp.min(UNITS.len() - 1);
    let value = bytes / 1024_f64.powi(exp as i32);
    format!("{:.2} {}", value, UNITS[exp])
}

fn get_dir_size(path: &str) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(path) {
        let entry = entry?;
        if entry.file_type().is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

// ---------------------------------------------------------------------
// ANSI color helpers (macro-generated instead of hand copy-pasted)
// ---------------------------------------------------------------------

macro_rules! color_fn {
    ($name:ident, $code:literal) => {
        fn $name(&self) -> String { format!("\x1b[{}m{}\x1b[0m", $code, self) }
    };
}

trait Colorize {
    fn red(&self) -> String;
    fn green(&self) -> String;
    fn yellow(&self) -> String;
    fn cyan(&self) -> String;
    fn bold(&self) -> String;
    fn underline(&self) -> String;
}

impl Colorize for str {
    color_fn!(red, "31");
    color_fn!(green, "32");
    color_fn!(yellow, "33");
    color_fn!(cyan, "36");
    color_fn!(bold, "1");
    color_fn!(underline, "4");
}

fn write_csv_row<W: std::io::Write>(writer: &mut W, fields: &[&str]) -> std::io::Result<()> {
    let escaped: Vec<String> = fields.iter().map(|f| {
        if f.contains(',') || f.contains('"') || f.contains('\n') {
            format!("\"{}\"", f.replace('"', "\"\""))
        } else {
            f.to_string()
        }
    }).collect();
    writeln!(writer, "{}", escaped.join(","))
}

// ---------------------------------------------------------------------
// DB helpers — every command was hand-rolling expand_tilde + Connection::open
// ---------------------------------------------------------------------

fn open_db(db_path: &str) -> rusqlite::Connection {
    let db_path = expand_tilde(db_path);
    rusqlite::Connection::open(&db_path).expect("Failed to open database")
}

/// Runs `sql` with `params` and collects each row into a 3-string tuple.
/// Covers the very common (artist, album, title)-shaped queries used all
/// over the CLI (search, list, export, info, ...).
fn query_triples(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Vec<(String, String, String)> {
    let mut stmt = conn.prepare(sql).expect("Failed to prepare statement");
    stmt.query_map(params, |row| {
        Ok((
            row.get::<_, String>(0).unwrap_or_default(),
            row.get::<_, String>(1).unwrap_or_default(),
            row.get::<_, String>(2).unwrap_or_default(),
        ))
    })
    .expect("Failed to execute query")
    .filter_map(Result::ok)
    .collect()
}

fn collect_existing_paths_column(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let sql = format!("SELECT path FROM {}", table);
    let mut stmt = conn.prepare(&sql).expect("Failed to prepare select statement");
    let mut rows = stmt.query([]).expect("Failed to query rows");
    let mut missing = Vec::new();
    while let Some(row) = rows.next().expect("Failed to fetch row") {
        let path: String = row.get(0).expect("Failed to get path");
        if !std::path::Path::new(&path).exists() {
            missing.push(path);
        }
    }
    missing
}

// ---------------------------------------------------------------------
// Progress bar + background ticker, wrapped so cleanup can't be forgotten
// (previously duplicated 3x with manual AtomicBool + thread::spawn).
// ---------------------------------------------------------------------

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
        .unwrap()
        .progress_chars("##-")
}

struct TickingBar {
    pb: Arc<ProgressBar>,
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TickingBar {
    fn new(len: u64) -> Self {
        let pb = Arc::new(ProgressBar::new(len));
        pb.set_style(bar_style());
        Self::from_bar(pb)
    }

    /// Wrap an already-configured progress bar (used for the multi-progress
    /// main bar in `compress_tracks`, whose style differs slightly).
    fn from_bar(pb: Arc<ProgressBar>) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);
        let pb_clone = Arc::clone(&pb);
        let handle = thread::spawn(move || {
            while running_clone.load(Ordering::Relaxed) {
                pb_clone.tick();
                thread::sleep(Duration::from_millis(100));
            }
        });
        Self { pb, running, handle: Some(handle) }
    }
}

impl Drop for TickingBar {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
    }
}

/// A command line interface for managing your music library
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Index the music library and playlists
    Index {
        /// Show what would be moved but don't actually move files
        #[arg(long, action = ArgAction::SetTrue)]
        dry_run: bool,
    },
    /// Find duplicate tracks
    Dupes {
        /// Interactively fix duplicates
        #[arg(long, action = ArgAction::SetTrue)]
        fix: bool,
    },
    /// List all tracks
    Ls {
        /// Search Query
        #[arg()]
        query: Option<String>,

        /// Filter by genre
        #[arg(long)]
        genre: Option<String>,
    },
    /// Export tracks to CSV
    Export,
    /// Show statistics
    Stats,
    /// Search library
    Search {
        /// Search Query
        #[arg(required = true)]
        query: String,
    },
    /// List all genres
    Genres,
    /// Compress audio files to mp3 for mobile sync
    Compress {
        /// Output directory for compressed files
        #[arg(long, short = 'o')]
        output_dir: String,

        /// Output format (mp3, aac, opus)
        #[arg(long, default_value = "mp3")]
        format: String,

        /// Audio bitrate (e.g., 128k, 192k, 256k)
        #[arg(long, default_value = "192k")]
        bitrate: String,

        /// Number of parallel jobs (default: number of CPU cores)
        #[arg(long, short = 'j')]
        jobs: Option<usize>,

        /// Force reconversion even if file already exists
        #[arg(long, action = ArgAction::SetTrue)]
        force: bool,

        /// Optional search query to filter tracks
        #[arg()]
        query: Option<String>,
    },
    /// Add lyrics to library
    Lyrics {
        #[arg()]
        query: Option<String>,

        /// Overwrite existing unsynced (plain) lyrics with synced lyrics
        #[arg(long, action = ArgAction::SetTrue)]
        overwrite: bool,

        /// Show which tracks would be updated without modifying any files
        #[arg(long, action = ArgAction::SetTrue)]
        dry_run: bool,
    },
    /// Show detailed info about a track
    Info {
        /// Search Query
        #[arg(required = true)]
        query: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct FilesConfig {
    music_directory: String,
    database_name: String,
    file_pattern: Option<String>,
    ignore: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Settings {
    files: FilesConfig,
    replace: Option<HashMap<String, String>>
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            files: FilesConfig {
                music_directory: "~/Music".to_string(),
                database_name: "~/Music/music_library.db".to_string(),
                file_pattern: Some("{album}/{albumartist} - {title}.{ext}".to_string()),
                ignore: Some(Vec::<String>::new())
            },
            replace: Some([
                (":" , "∶"),
                ("/" , "⁄"),
                ("*" , "∗"),
                ("?" , "？"),
                ("\"" , "″"),
                ("\\", "⧵"),
                ("." , "․"),
                ("|" , "ǀ"),
                ("<" , "‹"),
                (">" , "›"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<HashMap<String, String>>(),)
        }
    }
}

fn sanitize_filename_component(s: &str, replacements: &Option<HashMap<String, String>>) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        let mut replaced = false;
        if let Some(map) = replacements {
            for (from, to) in map {
                if from.chars().count() == 1 && c == from.chars().next().unwrap() {
                    result.push_str(to);
                    replaced = true;
                    break;
                }
            }
        }
        if !replaced {
            result.push(c);
        }
    }
    result
}

fn index_library(settings: &Settings, dry_run: bool) {
    let music_dir = expand_tilde(&settings.files.music_directory);
    let db_path = expand_tilde(&settings.files.database_name);
    let file_pattern = settings.files.file_pattern.as_deref();

    // Build ignore matcher
    let mut glob_builder = GlobSetBuilder::new();
    if let Some(ignore_patterns) = &settings.files.ignore {
        for pattern in ignore_patterns {
            if let Ok(glob) = Glob::new(pattern) {
                glob_builder.add(glob);
            }
        }
    }
    let glob_set = glob_builder.build().unwrap();

    let entries: Vec<_> = walkdir::WalkDir::new(&music_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            let rel_path = e.path().strip_prefix(&music_dir).unwrap_or(e.path());
            !glob_set.is_match(rel_path)
        })
        .collect();

    let mut conn = rusqlite::Connection::open(&db_path).expect("Failed to open database");

    conn.execute(
        "CREATE TABLE IF NOT EXISTS tracks (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            artist TEXT,
            album TEXT,
            albumartist TEXT,
            title TEXT,
            duration INTEGER,
            year INTEGER,
            genre TEXT
        )",
        [],
    ).expect("Failed to create table");

    let tx = conn.transaction().expect("Failed to start transaction");

    println!("Indexing music files in directory: {}", music_dir);

    let bar = TickingBar::new(entries.len() as u64);
    let pb = Arc::clone(&bar.pb);

    // Process files in parallel to read metadata
    let tracks: Vec<_> = entries.par_iter().filter_map(|entry| {
        let path = entry.path();
        let (artist, album, albumartist, title, year, genre) = match lofty::read_from_path(path) {
            Ok(tagged_file) => {
                let tag = tagged_file.primary_tag();
                let artist = tag.and_then(|t| t.get_string(&ItemKey::TrackArtist)).unwrap_or("").to_string();
                let albumartist = tag.and_then(|t| t.get_string(&ItemKey::AlbumArtist)).unwrap_or("").to_string();
                let album = tag.and_then(|t| t.get_string(&ItemKey::AlbumTitle)).unwrap_or("").to_string();
                let title = tag.and_then(|t| t.get_string(&ItemKey::TrackTitle)).unwrap_or("").to_string();
                let year = tag
                    .and_then(|t| t.get_string(&ItemKey::Year))
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0);
                let genre = tag.and_then(|t| t.get_string(&ItemKey::Genre)).unwrap_or("").to_string();
                (artist, album, albumartist, title, year, genre)
            }
            Err(_) => {
                pb.inc(1);
                return None;
            }
        };

        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext == "mp3" || ext == "flac" || ext == "wav" || ext == "m4a" {
                let mut path_str = path.to_string_lossy().to_string();

                if let Some(pattern) = file_pattern {
                    let new_rel_path = generate_path_from_pattern(
                        pattern,
                        &artist,
                        &albumartist,
                        &album,
                        &title,
                        ext,
                        &settings.replace,
                    );
                    let new_abs_path = std::path::Path::new(&music_dir).join(&new_rel_path);
                    if new_abs_path != path {
                        if dry_run {
                            println!(
                                "[dry-run] Would move:\n  from: {}\n  to:   {}",
                                path.display(),
                                new_abs_path.display()
                            );
                        } else {
                            if let Some(parent) = new_abs_path.parent() {
                                std::fs::create_dir_all(parent).ok();
                            }
                            std::fs::rename(path, &new_abs_path).ok();
                        }
                        path_str = new_abs_path.to_string_lossy().to_string();
                    }
                }

                pb.inc(1);
                return Some((path_str, artist, albumartist, album, title, year, genre));
            }
        }
        pb.inc(1);
        None
    }).collect();

    bar.pb.finish_with_message("Metadata reading complete");
    drop(bar); // stop ticker before starting the next progress bar

    println!("Inserting {} tracks into database...", tracks.len());
    let insert_bar = TickingBar::new(tracks.len() as u64);

    for (path_str, artist, albumartist, album, title, year, genre) in tracks {
        let result = tx.execute(
            "INSERT OR IGNORE INTO tracks (path, artist, albumartist, album, title, duration, year, genre) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            [
                &path_str as &dyn rusqlite::ToSql,
                &artist,
                &albumartist,
                &album,
                &title,
                &0.0 as &dyn rusqlite::ToSql,
                &year,
                &genre,
            ]
        );
        if let Ok(1) = result {
            insert_bar.pb.set_message(format!("Added: {}", path_str));
        }
        insert_bar.pb.inc(1);
    }
    insert_bar.pb.finish_with_message("Database insertion complete");
    drop(insert_bar);

    // Clean up missing files from database
    println!("Checking for missing files in database...");
    let to_remove = collect_existing_paths_column(&tx, "tracks");

    for path in &to_remove {
        println!("Removing missing file from database: {}", path);
        tx.execute("DELETE FROM tracks WHERE path = ?1", [path]).ok();
    }
    if !to_remove.is_empty() {
        println!("Removed {} missing files from database", to_remove.len());
    }

    tx.commit().expect("Failed to commit transaction");
}

fn find_duplicates(db_path: &str, fix: bool) {
    let conn = open_db(db_path);

    conn.execute(
        "CREATE TABLE IF NOT EXISTS kept_duplicates (
            id INTEGER PRIMARY KEY,
            artist TEXT NOT NULL,
            title TEXT NOT NULL,
            UNIQUE(artist, title)
        )",
        [],
    ).expect("Failed to create kept_duplicates table");

    let mut stmt = conn.prepare(
        "SELECT artist, title, COUNT(*) as count FROM tracks \
         WHERE artist != '' AND title != '' \
         GROUP BY artist, title HAVING count > 1",
    ).expect("Failed to prepare statement");

    let mut rows = stmt.query([]).expect("Failed to execute query");

    let mut found_duplicates = false;
    while let Some(row) = rows.next().expect("Failed to fetch row") {
        found_duplicates = true;
        let artist: String = row.get(0).expect("Failed to get artist");
        let title: String = row.get(1).expect("Failed to get title");
        let count: i32 = row.get(2).expect("Failed to get count");

        let is_kept: bool = conn.query_row(
            "SELECT 1 FROM kept_duplicates WHERE artist = ?1 AND title = ?2",
            [&artist, &title],
            |_| Ok(true)
        ).unwrap_or(false);

        let keep_tag = if is_kept { "[Keep All] ".green() } else { "".green() };
        println!("{}{} {}", keep_tag, format!("{} - {}", artist, title).cyan(),format!("(x{})", count).yellow());

        let mut path_stmt = conn.prepare(
            "SELECT id, path FROM tracks WHERE artist = ?1 AND title = ?2"
        ).expect("Failed to prepare path statement");

        let mut path_rows = path_stmt.query([&artist, &title]).expect("Failed to execute path query");
        let mut paths = Vec::new();
        while let Some(path_row) = path_rows.next().expect("Failed to fetch path row") {
            let id: i64 = path_row.get(0).expect("Failed to get id");
            let path: String = path_row.get(1).expect("Failed to get path");
            println!("  {}", path);
            paths.push((id, path));
        }

        if fix && paths.len() > 1 && !is_kept {
            let mut options: Vec<String> = vec!["Skip".to_string(), "Keep both".to_string()];
            options.extend(paths.iter().map(|(_, p)| p.clone()));
            match inquire::Select::new(
                &format!("Which file do you want to keep for '{} - {}'?", artist, title),
                options.clone(),
            ).prompt() {
                Ok(selected) if selected != "Skip" && selected != "Keep both" => {
                    for (id, path) in &paths {
                        if path != &selected {
                            conn.execute("DELETE FROM tracks WHERE id = ?1", [id]).expect("Failed to delete duplicate");
                            println!("  Removed duplicate from database: {}", path);
                            match std::fs::remove_file(path) {
                                Ok(_) => println!("  Deleted file from filesystem: {}", path),
                                Err(e) => eprintln!("  Failed to delete file '{}': {}", path, e),
                            }
                        }
                    }
                }
                Ok(selected) if selected == "Keep both" => {
                    conn.execute(
                        "INSERT OR IGNORE INTO kept_duplicates (artist, title) VALUES (?1, ?2)",
                        [&artist, &title],
                    ).expect("Failed to save kept duplicate");
                    println!("  Keeping all copies of '{} - {}' (won't show again)", artist, title);
                }
                Ok(_) | Err(_) => {
                    println!("  Skipped fixing '{} - {}'", artist, title);
                }
            }
        }
    }

    if !found_duplicates {
        println!("{}", "No duplicate tracks found.".green());
    }

    // Identify tracks where a lower quality version exists (FLAC > M4A > MP3)
    println!("\nTracks with lower quality duplicates (FLAC > M4A > MP3):");

    let mut stmt = conn.prepare(
        "SELECT artist, title, GROUP_CONCAT(path) as paths FROM tracks \
         WHERE artist != '' AND title != '' \
         GROUP BY artist, title HAVING COUNT(*) > 1"
    ).expect("Failed to prepare statement for quality check");

    let mut rows = stmt.query([]).expect("Failed to execute quality check query");

    let mut found_quality_dupes = false;
    while let Some(row) = rows.next().expect("Failed to fetch row") {
        let artist: String = row.get(0).expect("Failed to get artist");
        let title: String = row.get(1).expect("Failed to get title");
        let paths: String = row.get(2).expect("Failed to get paths");
        let files: Vec<&str> = paths.split(',').collect();

        fn quality_rank(ext: &str) -> u8 {
            match ext.to_lowercase().as_str() {
                "flac" => 1,
                "m4a" => 2,
                "mp3" => 3,
                _ => 100,
            }
        }

        let mut qualities: Vec<(u8, &str)> = files.iter()
            .filter_map(|p| {
                std::path::Path::new(p)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|ext| (quality_rank(ext), *p))
            })
            .collect();

        qualities.sort_by_key(|q| q.0);

        if qualities.len() > 1 && qualities[0].0 < qualities[1].0 {
            found_quality_dupes = true;
            println!("{}", format!("{} - {}", artist, title).cyan());
            for (rank, path) in &qualities {
                let label = match rank {
                    1 => "FLAC",
                    2 => "M4A",
                    3 => "MP3",
                    _ => "OTHER",
                };
                println!("  [{}] {}", label, path);
            }
        }
    }

    if !found_quality_dupes {
        println!("{}", "No lower quality duplicates found.".green());
    }
}

fn load_settings() -> Settings {
    let config_path = Path::new(&expand_tilde(CONFIG_PATH));
    if !config_path.exists() {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent).expect("Failed to create config directory");
        }
        let toml_string = toml::to_string_pretty(&Settings::default()).unwrap();
        fs::write(config_path, toml_string).expect("Failed to write default config");
    }
    app_config::Config::builder()
        .add_source(app_config::File::with_name(&expand_tilde(CONFIG_PATH)))
        .add_source(app_config::Environment::with_prefix("APP"))
        .build()
        .unwrap()
        .try_deserialize()
        .unwrap()
}

fn index_playlists(music_dir: &str, db_path: &str) {
    let db_path = expand_tilde(db_path);
    let mut conn = rusqlite::Connection::open(&db_path).expect("Failed to open database");
    conn.execute(
        "CREATE TABLE IF NOT EXISTS playlists (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            path TEXT NOT NULL UNIQUE
        )",
        [],
    ).expect("Failed to create playlists table");

    let tx = conn.transaction().expect("Failed to start transaction");

    let to_remove = collect_existing_paths_column(&tx, "playlists");
    for path in to_remove {
        println!("Removing missing playlist from database: {}", path);
        tx.execute("DELETE FROM playlists WHERE path = ?1", [&path]).ok();
    }

    println!("Indexing playlists in directory: {}", music_dir);

    // Load all tracks once to avoid repeated database queries for missing file suggestions
    let all_tracks: Vec<(String, String)> = {
        let tracks_conn = rusqlite::Connection::open(&db_path).expect("Failed to open database for tracks");
        let mut stmt = tracks_conn.prepare("SELECT title, path FROM tracks").expect("Failed to prepare statement");
        let mut rows = stmt.query([]).expect("Failed to execute query");
        let mut tracks = Vec::new();
        while let Some(row) = rows.next().expect("Failed to fetch row") {
            let title: String = row.get(0).expect("Failed to get title");
            let path: String = row.get(1).expect("Failed to get path");
            tracks.push((title, path));
        }
        tracks
    };

    for entry in walkdir::WalkDir::new(music_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        if let Some(ext) = path.extension() {
            if ext == "m3u" || ext == "m3u8" {
                let path_str = path.to_string_lossy();
                let name = path.file_stem().unwrap_or_default().to_string_lossy();
                tx.execute(
                    "INSERT OR IGNORE INTO playlists (name, path) VALUES (?1, ?2)",
                    [&name as &dyn rusqlite::ToSql, &path_str]
                ).ok();

                if let Ok(content) = std::fs::read_to_string(path) {
                    let playlist_dir = path.parent().unwrap_or_else(|| std::path::Path::new(""));
                    for line in content.lines() {
                        let trimmed = line.trim();
                        if trimmed.is_empty() || trimmed.starts_with('#') {
                            continue;
                        }
                        let song_path = if std::path::Path::new(trimmed).is_absolute() {
                            std::path::PathBuf::from(trimmed)
                        } else {
                            playlist_dir.join(trimmed)
                        };
                        if !song_path.exists() {
                            println!(
                                "Missing file in playlist '{}': {}",
                                name,
                                song_path.display()
                            );

                            let song_file_name = song_path.file_name().and_then(|f| f.to_str()).unwrap_or("");
                            let song_name = extract_song_name_from_filename(song_file_name)
                                .unwrap_or_else(|| song_file_name.to_string());
                            println!("  Suggested song name: {}", song_name);
                            if !song_file_name.is_empty() {
                                let mut suggestions = Vec::new();
                                for (candidate_title, candidate_path) in &all_tracks {
                                    let score = strsim::jaro(candidate_title, &song_name);
                                    suggestions.push((score, candidate_path.clone()));
                                }
                                suggestions.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                                let top_suggestions: Vec<_> = suggestions.into_iter().take(5).collect();
                                if !top_suggestions.is_empty() {
                                    let mut options: Vec<String> = top_suggestions
                                        .iter()
                                        .map(|(score, suggestion)| format!("({:.3}) {} ", score, suggestion))
                                        .collect();
                                    options.push("Remove".to_string());
                                    options.push("Skip".to_string());

                                    let (top_score, top_path) = &top_suggestions[0];
                                    if *top_score >= 0.9 {
                                        println!("  Auto-replacing '{}' with '{}' (similarity {:.3})", song_path.display(), top_path, top_score);
                                        update_playlist_line(&path_str, &song_path.display().to_string(), top_path).expect("Failed to update playlist");
                                    } else {
                                        match inquire::Select::new(
                                            &format!("Select a replacement for '{}':", song_file_name),
                                            options.clone(),
                                        ).prompt() {
                                            Ok(selected) if selected != "Skip" && selected != "Remove" => {
                                                let selected_path = selected
                                                    .split_once(')').map(|x| x.1)
                                                    .map(|s| s.trim())
                                                    .unwrap_or(&selected);
                                                println!("  Replacing '{}' with '{}'", song_path.display(), selected_path);
                                                update_playlist_line(&path_str, &song_path.display().to_string(), selected_path).expect("Failed to update playlist");
                                            }
                                            Ok(selected) if selected == "Remove" => {
                                                println!("  Removing '{}' from playlist", song_path.display());
                                                if let Err(e) = update_playlist_line(&path_str, &song_path.display().to_string(), "") {
                                                    eprintln!("Failed to update playlist file: {}", e);
                                                }
                                            }
                                            Ok(_) | Err(_) => {
                                                println!("  Skipped replacement for '{}'", song_path.display());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    tx.commit().expect("Failed to commit transaction");
}

/// (artist, album, title) rows filtered with a single LIKE pattern across
/// `column`, or unfiltered if `pattern` is None.
fn search_by_column(
    conn: &rusqlite::Connection,
    order_column: &str,
    filter_column: &str,
    pattern: Option<&str>,
) -> Vec<(String, String, String)> {
    match pattern {
        Some(p) => {
            let sql = format!(
                "SELECT artist, album, title FROM tracks WHERE {} LIKE ?1 ORDER BY {}, artist, album, title",
                filter_column, order_column
            );
            query_triples(conn, &sql, &[&format!("%{}%", p)])
        }
        None => {
            let sql = format!(
                "SELECT artist, album, title FROM tracks ORDER BY {}, artist, album, title",
                order_column
            );
            query_triples(conn, &sql, &[])
        }
    }
}

fn search_tracks(db_path: &str, query: Option<String>) {
    let conn = open_db(db_path);
    let q = query.as_deref();

    // (section title, column to filter/order by, index into the triple used for dedup, label)
    let sections: [(&str, &str, usize); 3] = [
        ("Tracks", "title", 2),
        ("Albums", "album", 1),
        ("Artists", "artist", 0),
    ];

    for (i, (heading, column, idx)) in sections.iter().enumerate() {
        if i == 0 {
            println!("{} (Track - Album - Artist)", "Tracks".bold().underline());
        } else {
            println!("\n{}", heading.bold().underline());
        }

        let results = search_by_column(&conn, column, column, q);

        if results.is_empty() {
            println!("{}", format!("No {} found.", heading.to_lowercase()).yellow());
            continue;
        }

        if *idx == 2 {
            // Tracks: flat list, title - album - artist
            for (artist, album, title) in results {
                println!("{} - {} - {}", title, album, artist);
            }
        } else {
            // Albums / Artists: unique values only
            let unique: std::collections::HashSet<String> = results
                .into_iter()
                .map(|t| match idx {
                    1 => t.1, // album
                    _ => t.0, // artist
                })
                .collect();
            for v in unique {
                println!("{}", v);
            }
        }
    }
}

fn print_grouped_tracks(results: Vec<(String, String, String)>) {
    if results.is_empty() {
        println!("{}", "No tracks found.".yellow());
        return;
    }
    let mut last_artist = String::new();
    let mut last_album = String::new();
    for (artist, album, title) in results {
        if artist != last_artist {
            println!("\n{}:", artist.bold());
            last_artist = artist.clone();
            last_album.clear();
        }
        if album != last_album {
            println!("  {}:", album.cyan());
            last_album = album.clone();
        }
        println!("    {}", title);
    }
}

fn list_tracks(db_path: &str, query: Option<String>, genre: Option<String>) {
    let conn = open_db(db_path);

    if let Some(ref g) = genre {
        println!("{} {}", "Genre:".bold(), g.cyan());
    }

    // Build the WHERE clause dynamically instead of a 4-way duplicated match.
    let mut clauses: Vec<&str> = Vec::new();
    let genre_pattern = genre.as_ref().map(|g| format!("%{}%", g));
    let query_pattern = query.as_ref().map(|q| format!("%{}%", q));

    if genre_pattern.is_some() {
        clauses.push("genre LIKE ?1");
    }
    if query_pattern.is_some() {
        clauses.push(if genre_pattern.is_some() {
            "(album LIKE ?2 OR artist LIKE ?2 OR title LIKE ?2)"
        } else {
            "(album LIKE ?1 OR artist LIKE ?1 OR title LIKE ?1)"
        });
    }

    let sql = if clauses.is_empty() {
        "SELECT artist, album, title FROM tracks ORDER BY artist, album, title".to_string()
    } else {
        format!(
            "SELECT artist, album, title FROM tracks WHERE {} ORDER BY artist, album, title",
            clauses.join(" AND ")
        )
    };

    let results = match (&genre_pattern, &query_pattern) {
        (Some(g), Some(q)) => query_triples(&conn, &sql, &[g, q]),
        (Some(g), None) => query_triples(&conn, &sql, &[g]),
        (None, Some(q)) => query_triples(&conn, &sql, &[q]),
        (None, None) => query_triples(&conn, &sql, &[]),
    };

    print_grouped_tracks(results);
}

fn export_tracks(db_path: &str) {
    let conn = open_db(db_path);
    let expanded_db_path = expand_tilde(db_path);

    let results = query_triples(&conn, "SELECT artist, album, title FROM tracks", &[]);

    let db_folder = std::path::Path::new(&expanded_db_path).parent().unwrap_or_else(|| std::path::Path::new("."));
    let csv_path = db_folder.join("tracks_export.csv");
    let mut file = std::fs::File::create(&csv_path).expect("Failed to create CSV file");

    write_csv_row(&mut file, &["Artist", "Album", "Title"]).expect("Failed to write CSV header");

    for (artist, album, title) in results {
        write_csv_row(&mut file, &[&artist, &album, &title]).expect("Failed to write CSV record");
    }

    println!("Exported tracks to {}", csv_path.display());
}

fn get_stats(music_dir: &str, db_path: &str) {
    let conn = open_db(db_path);

    let total_tracks: i64 = conn.query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0)).unwrap_or(0);
    let total_artists: i64 = conn.query_row("SELECT COUNT(DISTINCT artist) FROM tracks", [], |row| row.get(0)).unwrap_or(0);
    let total_albums: i64 = conn.query_row("SELECT COUNT(DISTINCT album) FROM tracks", [], |row| row.get(0)).unwrap_or(0);

    // update durations if they are zero
    let mut stmt = conn.prepare("SELECT id, path, duration FROM tracks WHERE duration = 0").expect("Failed to prepare statement");
    let mut rows = stmt.query([]).expect("Failed to execute query");
    let mut rows_vec = Vec::new();
    while let Some(row) = rows.next().expect("Failed to fetch row") {
        let id: i64 = row.get(0).expect("Failed to get id");
        let path: String = row.get(1).expect("Failed to get path");
        rows_vec.push((id, path));
    }
    drop(rows);
    drop(stmt);

    let bar = TickingBar::new(rows_vec.len() as u64);

    for (id, path) in rows_vec {
        let duration: f64 = get_duration_with_lofty(std::path::Path::new(&path)) as f64;
        if duration > 0.0 {
            conn.execute("UPDATE tracks SET duration = ?1 WHERE id = ?2", [duration, id as f64]).expect("Failed to update duration");
        }
        bar.pb.inc(1);
        bar.pb.set_message(path.to_string());
    }
    bar.pb.finish_with_message("Duration update complete");
    drop(bar);

    let total_duration: f64 = conn.query_row(
        "SELECT SUM(duration) FROM tracks",
        [],
        |row| row.get(0)
    ).unwrap_or(0.0);

    fn format_duration(secs: f64) -> String {
        let months: f64 = secs / 2592000.0;
        let weeks: f64 = secs / 604800.0;
        let days: f64 = secs / 86400.0;
        let hours: f64 = secs / 3600.0;
        let minutes: f64 = secs / 60.0;
        if months > 1.0 {
            format!("{:.2} months", months)
        } else if weeks > 1.0 {
            format!("{:.2} weeks", weeks)
        } else if days > 1.0 {
            format!("{:.2} days", days)
        } else if hours > 1.0 {
            format!("{:.2} hours", hours)
        } else if minutes > 1.0 {
            format!("{:.2} minutes", minutes)
        } else {
            format!("{:.2} seconds", secs)
        }
    }

    let folder_size: String = format_bytes(get_dir_size(music_dir).unwrap() as f64);

    println!("Total tracks: {}", total_tracks);
    println!("Total artists: {}", total_artists);
    println!("Total albums: {}", total_albums);
    println!("Total size: {}", folder_size);
    println!("Total time: {}", format_duration(total_duration));

    // --- Date Histogram ---
    println!("\nTracks by Year:");
    let mut stmt = conn.prepare(
        "SELECT year, COUNT(*) FROM tracks WHERE year IS NOT NULL AND year > 0 GROUP BY year ORDER BY year"
    ).expect("Failed to prepare year histogram statement");
    let mut rows = stmt.query([]).expect("Failed to execute year histogram query");

    let mut year_counts = Vec::new();
    let mut max_count = 0;
    while let Some(row) = rows.next().expect("Failed to fetch year row") {
        let year: i64 = row.get(0).unwrap_or(0);
        let count: i64 = row.get(1).unwrap_or(0);
        if count > max_count {
            max_count = count;
        }
        year_counts.push((year, count));
    }

    for (year, count) in year_counts {
        let bar_len = if max_count > 0 { (count * 40 / max_count) as usize } else { 0 };
        let bar = "█".repeat(bar_len);
        println!("{:4}: {:4} {}", year, count, bar);
    }
}

fn get_duration_with_lofty(path: &std::path::Path) -> i64 {
    match lofty::read_from_path(path) {
        Ok(tagged_file) => {
            tagged_file.properties().duration().as_secs() as i64
        }
        Err(_) => 0,
    }
}

fn extract_song_name_from_filename(filename: &str) -> Option<String> {
    let file_stem = std::path::Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())?;
    for sep in [" - ", " － "] {
        let parts: Vec<&str> = file_stem.split(sep).collect();
        if parts.len() > 1 {
            return Some(parts[1].to_string());
        }
    }
    None
}

fn update_playlist_line(playlist_path: &str, target_line: &str, new_line: &str) -> std::io::Result<()> {
    let content = std::fs::read_to_string(playlist_path)?;
    let playlist_dir = Path::new(playlist_path).parent().unwrap_or_else(|| Path::new(""));

    let target_path = Path::new(target_line);
    let target_rel = target_path.strip_prefix(playlist_dir).unwrap_or(target_path);

    let new_path = Path::new(new_line);
    let new_rel = new_path.strip_prefix(playlist_dir).unwrap_or(new_path);

    let mut replaced = false;
    let mut new_lines = Vec::new();
    for line in content.lines() {
        let line_path = Path::new(line.trim());
        let line_rel = line_path.strip_prefix(playlist_dir).unwrap_or(line_path);

        if !replaced && line_rel == target_rel {
            new_lines.push(new_rel.to_string_lossy().to_string());
            replaced = true;
        } else {
            new_lines.push(line.to_string());
        }
    }
    let new_content = new_lines.join("\n");
    println!("Updating playlist: {} -> {}", target_rel.display(), new_rel.display());
    if !replaced {
        println!("{}", format!("Warning: Target line '{}' not found in playlist '{}'", target_rel.display(), playlist_path).yellow());
        return Ok(());
    }
    std::fs::write(playlist_path, new_content)?;
    Ok(())
}

fn generate_path_from_pattern(
    pattern: &str,
    artist: &str,
    albumartist: &str,
    album: &str,
    title: &str,
    ext: &str,
    replacements: &Option<HashMap<String, String>>,
) -> String {
    let artist_sanitized = sanitize_filename_component(artist, replacements);
    let albumartist_sanitized = if albumartist.trim().is_empty() || albumartist.trim().eq_ignore_ascii_case("Various Artists") {
        sanitize_filename_component(artist, replacements)
    } else {
        sanitize_filename_component(albumartist, replacements)
    };
    let album_sanitized = sanitize_filename_component(album, replacements);
    let title_sanitized = sanitize_filename_component(title, replacements);
    let ext_sanitized = sanitize_filename_component(ext, replacements);

    pattern
        .replace("{artist}", &artist_sanitized)
        .replace("{albumartist}", &albumartist_sanitized)
        .replace("{album}", &album_sanitized)
        .replace("{title}", &title_sanitized)
        .replace("{ext}", &ext_sanitized)
}

fn list_genres(db_path: &str) {
    let conn = open_db(db_path);

    let mut stmt = conn.prepare(
        "SELECT genre FROM tracks WHERE genre != ''"
    ).expect("Failed to prepare statement");

    let mut rows = stmt.query([]).expect("Failed to execute query");

    let mut counts: HashMap<String, usize> = HashMap::new();
    while let Some(row) = rows.next().expect("Failed to fetch row") {
        let genre_str: String = row.get(0).unwrap_or_default();
        for genre in genre_str.split(',') {
            let genre = genre.trim().to_string();
            if !genre.is_empty() {
                *counts.entry(genre).or_insert(0) += 1;
            }
        }
    }

    if counts.is_empty() {
        println!("{}", "No genres found.".yellow());
        return;
    }

    let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    for (genre, count) in sorted {
        println!("{:<30} {}", genre.bold(), format!("({} tracks)", count).yellow());
    }
}

fn export_playlists_for_compressed(
    conn: &rusqlite::Connection,
    music_dir: &str,
    output_dir: &str,
    format: &str,
) {
    let mut stmt = match conn.prepare("SELECT name, path FROM playlists") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to query playlists: {}", e);
            return;
        }
    };

    let playlist_results: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    if playlist_results.is_empty() {
        println!("No playlists found.");
        return;
    }

    println!("Found {} playlists to export", playlist_results.len());

    for (name, playlist_path) in playlist_results {
        let content = match std::fs::read_to_string(&playlist_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to read playlist '{}': {}", name, e);
                continue;
            }
        };

        let playlist_path_obj = Path::new(&playlist_path);
        let playlist_dir = playlist_path_obj.parent().unwrap_or_else(|| Path::new(""));

        let mut updated_lines = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim();

            if trimmed.is_empty() || trimmed.starts_with('#') {
                updated_lines.push(line.to_string());
                continue;
            }

            let song_path = if Path::new(trimmed).is_absolute() {
                PathBuf::from(trimmed)
            } else {
                playlist_dir.join(trimmed)
            };

            let relative_to_music = match song_path.strip_prefix(music_dir) {
                Ok(rel) => rel,
                Err(_) => {
                    eprintln!(
                        "Warning: Path '{}' in playlist '{}' is not under music directory, skipping",
                        song_path.display(),
                        name
                    );
                    updated_lines.push(line.to_string());
                    continue;
                }
            };

            let mut new_path = PathBuf::new();
            new_path.push(relative_to_music);
            new_path.set_extension(format);

            let new_path_str = new_path.to_string_lossy().to_string();
            updated_lines.push(new_path_str);
        }

        let output_playlist_path = PathBuf::from(output_dir).join(format!("{}.m3u", name));

        if let Some(parent) = output_playlist_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("Failed to create directory for playlist '{}': {}", name, e);
                continue;
            }
        }

        match std::fs::write(&output_playlist_path, updated_lines.join("\n") + "\n") {
            Ok(_) => println!("  ✓ Exported playlist: {}", name),
            Err(e) => eprintln!("  ✗ Failed to write playlist '{}': {}", name, e),
        }
    }
}

fn compress_tracks(
    music_dir: &str,
    db_path: &str,
    output_dir: &str,
    format: &str,
    bitrate: &str,
    jobs: Option<usize>,
    force: bool,
    query: Option<String>,
) {
    let db_path_expanded = expand_tilde(db_path);
    let music_dir = expand_tilde(music_dir);
    let output_dir = expand_tilde(output_dir);

    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("{}", "Error: ffmpeg is not installed or not in PATH".red());
        eprintln!("Please install ffmpeg to use the compress command");
        return;
    }

    if let Some(num_jobs) = jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_jobs)
            .build_global()
            .ok();
    }

    let conn = rusqlite::Connection::open(&db_path_expanded).expect("Failed to open database");

    let (query_sql, pattern): (&str, Option<String>) = if let Some(ref q) = query {
        (
            "SELECT path FROM tracks WHERE album LIKE ?1 OR artist LIKE ?1 OR title LIKE ?1",
            Some(format!("%{}%", q))
        )
    } else {
        ("SELECT path FROM tracks", None)
    };

    let mut stmt = conn.prepare(query_sql).expect("Failed to prepare statement");
    let paths: Vec<String> = match &pattern {
        Some(p) => stmt
            .query_map([p], |row| row.get::<_, String>(0))
            .expect("Failed to execute query")
            .filter_map(Result::ok)
            .collect(),
        None => stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("Failed to execute query")
            .filter_map(Result::ok)
            .collect(),
    };
    drop(stmt);

    if paths.is_empty() {
        println!("{}", "No tracks found to compress.".yellow());
        return;
    }

    let thread_count = jobs.unwrap_or_else(|| num_cpus::get());
    println!(
        "Compressing {} tracks to {} as {} at {} bitrate (using {} threads)...\n",
        paths.len(), output_dir, format, bitrate, thread_count
    );

    // Set up multi-progress display
    let multi_progress = Arc::new(MultiProgress::new());

    let main_pb_raw = Arc::new(multi_progress.add(ProgressBar::new(paths.len() as u64)));
    main_pb_raw.set_style(
        ProgressStyle::with_template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({per_sec}) - {msg}")
            .unwrap()
            .progress_chars("##-"),
    );

    let worker_count = thread_count.min(8);
    let worker_bars: Vec<_> = (0..worker_count)
        .map(|i| {
            let pb = multi_progress.add(ProgressBar::new_spinner());
            pb.set_style(
                ProgressStyle::with_template(&format!("  Worker {}: {{spinner}} {{msg}}", i + 1))
                    .unwrap()
            );
            pb.set_message("Idle".to_string());
            Arc::new(pb)
        })
        .collect();

    let compressed_count = Arc::new(Mutex::new(0));
    let skipped_count = Arc::new(Mutex::new(0));
    let failed_count = Arc::new(Mutex::new(0));
    let failed_files = Arc::new(Mutex::new(Vec::new()));

    // The worker spinners also need ticking; ride along on the same ticker
    // thread as the main bar instead of a bespoke loop.
    let worker_bars_ticker = worker_bars.clone();
    let bar = TickingBar::from_bar(Arc::clone(&main_pb_raw));
    let ticker_running_for_workers = Arc::clone(&bar.running);
    let worker_ticker_handle = {
        let running = Arc::clone(&ticker_running_for_workers);
        thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                for wb in &worker_bars_ticker {
                    wb.tick();
                }
                thread::sleep(Duration::from_millis(100));
            }
        })
    };

    let main_pb_clone = Arc::clone(&main_pb_raw);
    let worker_bars_clone = worker_bars.clone();

    paths.par_iter().for_each(|source_path| {
        let source = std::path::Path::new(&source_path);

        if !source.exists() {
            *failed_count.lock().unwrap() += 1;
            failed_files.lock().unwrap().push(source_path.clone());
            main_pb_clone.inc(1);
            return;
        }

        let relative_path = source.strip_prefix(&music_dir).unwrap_or(source);

        let mut output_path = std::path::PathBuf::from(&output_dir);
        output_path.push(relative_path);
        output_path.set_extension(format);

        if let Some(parent) = output_path.parent() {
            if let Err(_) = std::fs::create_dir_all(parent) {
                *failed_count.lock().unwrap() += 1;
                failed_files.lock().unwrap().push(source_path.clone());
                main_pb_clone.inc(1);
                return;
            }
        }

        if !force && output_path.exists() {
            *skipped_count.lock().unwrap() += 1;
            main_pb_clone.inc(1);
            return;
        }

        let file_name = source.file_name().unwrap_or_default().to_string_lossy().to_string();
        let worker_idx = rayon::current_thread_index().unwrap_or(0) % worker_count;
        let worker_bar = &worker_bars_clone[worker_idx];

        worker_bar.set_message(format!("🎵 {}", file_name));
        worker_bar.tick();

        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.arg("-i").arg(source_path);

        match format {
            "mp3" => { cmd.arg("-c:a").arg("libmp3lame"); }
            "aac" | "m4a" => { cmd.arg("-c:a").arg("aac"); }
            "opus" => { cmd.arg("-c:a").arg("libopus"); }
            _ => { cmd.arg("-c:a").arg("libmp3lame"); }
        }

        cmd.arg("-b:a")
            .arg(bitrate)
            .arg("-map")
            .arg("0")
            .arg("-c:v")
            .arg("copy")
            .arg("-y")
            .arg(&output_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let status = cmd.status();

        match status {
            Ok(exit_status) if exit_status.success() => {
                // Copy Lyrics tag
                copy_lyrics_tag(&output_path);
                *compressed_count.lock().unwrap() += 1;
                worker_bar.set_message(format!("✓ {}", file_name));
            }
            _ => {
                *failed_count.lock().unwrap() += 1;
                failed_files.lock().unwrap().push(source_path.clone());
                worker_bar.set_message(format!("✗ {}", file_name));
            }
        }

        main_pb_clone.inc(1);

        let compressed = *compressed_count.lock().unwrap();
        let skipped = *skipped_count.lock().unwrap();
        let failed = *failed_count.lock().unwrap();
        main_pb_clone.set_message(format!(
            "✓ {} ⊘ {} ✗ {}",
            compressed, skipped, failed
        ));
    });

    drop(bar); // stops main ticker; shared flag also signals the worker ticker below
    let _ = &ticker_running_for_workers; // kept alive until here for clarity
    worker_ticker_handle.join().ok();

    main_pb_raw.finish_with_message("Compression complete");

    for wb in &worker_bars {
        wb.finish_and_clear();
    }

    let compressed = *compressed_count.lock().unwrap();
    let skipped = *skipped_count.lock().unwrap();
    let failed = *failed_count.lock().unwrap();
    let failed_list = failed_files.lock().unwrap();

    println!("\nSummary:");
    println!("  Compressed: {}", compressed.to_string().green());
    println!("  Skipped (already exist): {}", skipped.to_string().yellow());
    println!("  Failed: {}", failed.to_string().red());

    if !failed_list.is_empty() {
        println!("\nFailed files:");
        for path in failed_list.iter() {
            println!("  {}", path.red());
        }
    }

    println!("\nExporting playlists...");
    export_playlists_for_compressed(&conn, &music_dir, &output_dir, format);
}

fn copy_lyrics_tag(path: &Path) {
    let Ok(mut tagged_file) = lofty::read_from_path(path) else { return; };
    let Some(tag) = tagged_file.primary_tag_mut() else { return; };
    let Some((key, lyrics)) = tag
        .items()
        .find(|i| matches!(i.key(), ItemKey::Unknown(k) if k.to_lowercase().starts_with("lyrics")))
            .and_then(|i| Some((i.key().clone(), i.value().text()?.to_string()))) else {return;};
    tag.remove_key(&key);
    tag.insert_text(ItemKey::Lyrics, lyrics);
    let _ = tag.save_to_path(path, WriteOptions::default());
}

// `fetch_lyrics`'s direct-lookup and `search_lyrics`'s fuzzy-search results
// had identical shapes (LrcLibResult / LrcLibSearchResult) — merged into one.
#[derive(Deserialize, Debug)]
struct LrcLibResult {
    #[serde(rename = "trackName")]
    track_name: String,
    #[serde(rename = "artistName")]
    artist_name: String,
    #[serde(rename = "plainLyrics")]
    plain_lyrics: Option<String>,
    #[serde(rename = "syncedLyrics")]
    synced_lyrics: Option<String>,
}

async fn fetch_lyrics(
    client: &reqwest::Client,
    artist: &str,
    title: &str,
    album: Option<&str>,
    duration_secs: Option<u32>,
) -> Result<Option<LrcLibResult>, reqwest::Error> {
    let mut req = client
        .get("https://lrclib.net/api/get")
        .query(&[("artist_name", artist), ("track_name", title)]);

    if let Some(a) = album {
        req = req.query(&[("album_name", a)]);
    }
    if let Some(d) = duration_secs {
        req = req.query(&[("duration", d.to_string())]);
    }

    let resp: reqwest::Response = req.send().await?;
    if resp.status().is_success() {
        Ok(Some(resp.json::<LrcLibResult>().await?))
    } else {
        let _text = resp.text().await.unwrap_or_default();
        Ok(None) // fall back to a search endpoint or another source
    }
}

fn is_synced_lyrics(s: &str) -> bool {
    // LRC-style lines look like "[00:12.34]lyric text"
    s.lines().any(|l| {
        let l = l.trim();
        l.starts_with('[')
            && l[1..].chars().next().is_some_and(|c| c.is_ascii_digit())
    })
}

async fn search_lyrics(
    client: &reqwest::Client,
    artist: &str,
    title: &str,
) -> Result<Option<LrcLibResult>, reqwest::Error> {
    let resp = client
        .get("https://lrclib.net/api/search")
        .query(&[("artist_name", artist), ("track_name", title)])
        .send()
        .await?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let results: Vec<LrcLibResult> = resp.json().await?;
    if results.is_empty() {
        return Ok(None);
    }

    let best = results.into_iter().max_by(|a, b| {
        let a_synced = a.synced_lyrics.as_deref().is_some_and(|s| !s.trim().is_empty());
        let b_synced = b.synced_lyrics.as_deref().is_some_and(|s| !s.trim().is_empty());
        let score_a = strsim::jaro(&a.artist_name, artist) + strsim::jaro(&a.track_name, title);
        let score_b = strsim::jaro(&b.artist_name, artist) + strsim::jaro(&b.track_name, title);
        (a_synced, score_a)
            .partial_cmp(&(b_synced, score_b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(best)
}

struct LyricsCandidate {
    path: String,
    artist: String,
    album: String,
    title: String,
    duration: i64,
    has_lyrics: bool,
    is_synced: bool,
}

/// Writes `lyrics` into the file's Lyrics tag and saves it, reporting
/// success/failure via the shared progress bar. Used for both the
/// synced- and plain-lyrics cases, which previously duplicated this
/// entire read/insert/save/report sequence.
fn write_lyrics_tag(
    path: &std::path::Path,
    lyrics: String,
    artist: &str,
    title: &str,
    pb: &ProgressBar,
    kind: &str,
) -> bool {
    match lofty::read_from_path(path) {
        Ok(mut tagged_file) => {
            if let Some(tag) = tagged_file.primary_tag_mut() {
                tag.insert_text(ItemKey::Lyrics, lyrics);
                match tag.save_to_path(path, WriteOptions::default()) {
                    Ok(_) => {
                        pb.set_message(format!("✓ tagged {} lyrics for {} - {}", kind, artist, title));
                        true
                    }
                    Err(e) => {
                        eprintln!("Failed to write lyrics to {}: {}", path.display(), e);
                        false
                    }
                }
            } else {
                false
            }
        }
        Err(e) => {
            eprintln!("Failed to read {}: {}", path.display(), e);
            false
        }
    }
}

fn add_lyrics(
    music_dir: &str,
    db_path: &str,
    jobs: Option<usize>,
    query: Option<String>,
    overwrite: bool,
    dry_run: bool,
) {
    let _music_dir = expand_tilde(music_dir);
    let conn = open_db(db_path);

    let (query_sql, pattern) = if let Some(ref q) = query {
        (
            "SELECT path, artist, album, title, duration FROM tracks \
             WHERE album LIKE ?1 OR artist LIKE ?1 OR title LIKE ?1",
            Some(format!("%{}%", q)),
        )
    } else {
        ("SELECT path, artist, album, title, duration FROM tracks", None)
    };

    let mut stmt = conn.prepare(query_sql).expect("Failed to prepare statement");
    let mut rows = if let Some(ref p) = pattern {
        stmt.query([p]).expect("Failed to execute query")
    } else {
        stmt.query([]).expect("Failed to execute query")
    };

    let mut raw_tracks = Vec::new();
    while let Some(row) = rows.next().expect("Failed to fetch row") {
        raw_tracks.push((
            row.get::<_, String>(0).unwrap_or_default(),
            row.get::<_, String>(1).unwrap_or_default(),
            row.get::<_, String>(2).unwrap_or_default(),
            row.get::<_, String>(3).unwrap_or_default(),
            row.get::<_, i64>(4).unwrap_or_default(),
        ));
    }
    drop(rows);
    drop(stmt);

    if raw_tracks.is_empty() {
        println!("{}", "No tracks found.".yellow());
        return;
    }

    println!("Checking existing lyrics tags for {} tracks...", raw_tracks.len());
    let mut candidates = Vec::new();
    for (path, artist, album, title, duration) in raw_tracks {
        let p = std::path::Path::new(&path);
        if !p.exists() {
            continue;
        }

        let (has_lyrics, is_synced) = match lofty::read_from_path(p) {
            Ok(tagged_file) => {
                if let Some(lyrics) = tagged_file
                    .primary_tag()
                    .and_then(|t| t.get_string(&ItemKey::Lyrics))
                {
                    (true, is_synced_lyrics(lyrics))
                } else {
                    (false, false)
                }
            }
            Err(_) => (false, false),
        };

        let needs_update = !has_lyrics || !is_synced && overwrite;

        if needs_update {
            candidates.push(LyricsCandidate {
                path,
                artist,
                album,
                title,
                duration,
                has_lyrics,
                is_synced,
            });
        }
    }

    if candidates.is_empty() {
        println!("{}", "No tracks need lyrics updates.".green());
        return;
    }

    println!("\n{} tracks will be updated:", candidates.len());
    for c in &candidates {
        let reason = if !c.has_lyrics {
            "missing lyrics".to_string()
        } else if !c.is_synced {
            "unsynced lyrics, will overwrite".to_string()
        } else {
            "".to_string()
        };
        println!("  {} - {} ({})", c.artist.cyan(), c.title, reason.yellow());
    }

    if dry_run {
        println!(
            "\n{}",
            "[dry-run] No files were modified. Re-run without --dry-run to apply.".yellow()
        );
        return;
    }

    if let Some(num_jobs) = jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_jobs)
            .build_global()
            .ok();
    }

    let bar = TickingBar::new(candidates.len() as u64);

    let updated_count = Arc::new(Mutex::new(0usize));
    let not_found_count = Arc::new(Mutex::new(0usize));
    let failed_count = Arc::new(Mutex::new(0usize));

    // reqwest needs an async runtime; we create one and block_on it from each
    // rayon worker thread (safe — these worker threads are separate from the
    // tokio runtime's own threads).
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    let client = reqwest::Client::builder()
            .user_agent("apollo-music/0.1 (https://github.com/rkliman/apollo-music)")
            .build()
            .expect("failed to build client");

    let pb = Arc::clone(&bar.pb);
    candidates.par_iter().for_each(|c| {
        let path = std::path::Path::new(&c.path);
        let duration_secs = if c.duration > 0 { Some(c.duration as u32) } else { None };

        let mut result = rt.block_on(fetch_lyrics(
            &client,
            &normalize_text(&c.artist),
            &normalize_text(&c.title),
            if c.album.is_empty() { None } else { Some(c.album.as_str()) },
            duration_secs,
        ));

        let needs_fallback = match &result {
            Ok(None) => true,
            Ok(Some(lrc)) => lrc.synced_lyrics.as_deref().is_none_or(|s| s.trim().is_empty()),
            Err(_) => false,
        };

        if needs_fallback {
            if let Ok(Some(found)) = rt.block_on(search_lyrics(
                &client,
                &normalize_text(&c.artist),
                &normalize_text(&c.title),
            )) {
                result = Ok(Some(found));
            }
        }

        match result {
            Ok(Some(lrc)) => {
                if let Some(synced) = lrc.synced_lyrics.filter(|s| !s.trim().is_empty()) {
                    if write_lyrics_tag(path, synced, &c.artist, &c.title, &pb, "synced") {
                        *updated_count.lock().unwrap() += 1;
                    } else {
                        *failed_count.lock().unwrap() += 1;
                    }
                } else if let Some(plain) = lrc.plain_lyrics.filter(|s| !s.trim().is_empty()) {
                    if write_lyrics_tag(path, plain, &c.artist, &c.title, &pb, "plain") {
                        *updated_count.lock().unwrap() += 1;
                    } else {
                        *failed_count.lock().unwrap() += 1;
                    }
                }
            }
            Ok(None) => {
                pb.set_message(format!("⊘ not found: {}", c.title));
                *not_found_count.lock().unwrap() += 1;
            }
            Err(e) => {
                eprintln!("Error fetching lyrics for '{}': {}", c.title, e);
                *failed_count.lock().unwrap() += 1;
            }
        }
        pb.inc(1);
    });

    bar.pb.finish_with_message("Lyrics update complete");
    drop(bar);

    let updated = *updated_count.lock().unwrap();
    let not_found = *not_found_count.lock().unwrap();
    let failed = *failed_count.lock().unwrap();

    println!("\nSummary:");
    println!("  Updated: {}", updated.to_string().green());
    println!("  No synced lyrics found: {}", not_found.to_string().yellow());
    println!("  Failed: {}", failed.to_string().red());
}

fn get_info(db_path: &str, query: &str) {
    let conn = open_db(db_path);

    let pattern = format!("%{}%", query);
    let mut stmt = conn.prepare(
        "SELECT artist, album, title, path FROM tracks \
         WHERE artist LIKE ?1 OR album LIKE ?1 OR title LIKE ?1 \
         ORDER BY artist, album, title LIMIT 25"
    ).expect("Failed to prepare statement");

    let mut rows = stmt.query([&pattern]).expect("Failed to execute query");
    let mut results = Vec::new();
    while let Some(row) = rows.next().expect("Failed to fetch row") {
        let artist: String = row.get(0).unwrap_or_default();
        let album: String = row.get(1).unwrap_or_default();
        let title: String = row.get(2).unwrap_or_default();
        let path: String = row.get(3).unwrap_or_default();
        results.push((artist, album, title, path));
    }

    if results.is_empty() {
        println!("{}", "No matching tracks found.".yellow());
        return;
    }

    let options: Vec<String> = results
        .iter()
        .map(|(artist, album, title, _)| format!("{} - {}  [{}]", artist, title, album))
        .collect();

    let selection = inquire::Select::new("Select a track to view info:", options.clone())
        .prompt();

    let selected_idx = match selection {
        Ok(sel) => options.iter().position(|o| o == &sel),
        Err(_) => None,
    };

    let Some(idx) = selected_idx else {
        println!("{}", "No track selected.".yellow());
        return;
    };

    let (_, _, _, path) = &results[idx];
    print_track_info(path);
}

fn print_track_info(path_str: &str) {
    let path = std::path::Path::new(path_str);
    println!("\n{}", path_str.bold().underline());

    if !path.exists() {
        println!("{}", "File does not exist on disk.".red());
        return;
    }

    let tagged_file = match lofty::read_from_path(path) {
        Ok(f) => f,
        Err(e) => {
            println!("{}", format!("Failed to read file tags: {}", e).red());
            return;
        }
    };

    println!("{} {:?}", "File type:".bold(), tagged_file.file_type());

    let properties = tagged_file.properties();
    println!("\n{}", "Audio properties:".bold().underline());
    println!("  {:<20} {:?}", "Duration:", properties.duration());
    if let Some(br) = properties.overall_bitrate() {
        println!("  {:<20} {} kbps", "Overall bitrate:", br);
    }
    if let Some(br) = properties.audio_bitrate() {
        println!("  {:<20} {} kbps", "Audio bitrate:", br);
    }
    if let Some(sr) = properties.sample_rate() {
        println!("  {:<20} {} Hz", "Sample rate:", sr);
    }
    if let Some(bd) = properties.bit_depth() {
        println!("  {:<20} {}", "Bit depth:", bd);
    }
    if let Some(ch) = properties.channels() {
        println!("  {:<20} {}", "Channels:", ch);
    }

    match tagged_file.primary_tag() {
        Some(tag) => {
            println!("\n{}", "Tags:".bold().underline());
            for item in tag.items() {
                let key_str = format!("{:?}", item.key());
                let value_str = match item.value() {
                    lofty::tag::ItemValue::Text(s) => s.clone(),
                    lofty::tag::ItemValue::Locator(s) => s.clone(),
                    lofty::tag::ItemValue::Binary(b) => format!("<binary, {} bytes>", b.len()),
                };
                println!("  {:<25} {}", key_str.cyan(), value_str);
            }

            let pictures = tag.pictures();
            if !pictures.is_empty() {
                println!("\n{}", "Artwork:".bold().underline());
                for pic in pictures {
                    println!(
                        "  {:?}  {:?}  ({} bytes)",
                        pic.pic_type(),
                        pic.mime_type(),
                        pic.data().len()
                    );
                }
            }
        }
        None => {
            println!("\n{}", "No tags found on this file.".yellow());
        }
    }
}

fn main() {
    // (1) Load settings. creates config file if missing
    let settings = load_settings();

    let music_dir = expand_tilde(&settings.files.music_directory);
    let db_path = expand_tilde(&settings.files.database_name);

    // (2) Ensure the music directory exists
    if !Path::new(&music_dir).exists() {
        fs::create_dir_all(&music_dir).expect("Failed to create music directory");
        println!("Created music directory: {}", music_dir);
    }

    // Ensure the database's parent directory exists
    let db_folder = Path::new(&db_path)
        .parent()
        .expect("Database path has no parent directory");
    if !db_folder.exists() {
        fs::create_dir_all(db_folder).expect("Failed to create database directory");
    }

    // (3) Ensure the database file exists
    if !Path::new(&db_path).exists() {
        let conn = rusqlite::Connection::open(&db_path)
            .expect("Failed to create database file");
        conn.execute(
            "CREATE TABLE IF NOT EXISTS tracks (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                artist TEXT,
                album TEXT,
                albumartist TEXT,
                title TEXT,
                duration INTEGER,
                year INTEGER,
                genre TEXT
            )",
            [],
        ).expect("Failed to initialize database schema");
        println!("Created database: {}", db_path);
    }

    let args = Cli::parse();
    match args.command {
        Commands::Index { dry_run } => {
            index_library(&settings, dry_run);
            index_playlists(&music_dir, &db_path);
        }
        Commands::Dupes { fix } => {
            find_duplicates(&db_path, fix);
        }
        Commands::Ls { query, genre } => {
            list_tracks(&db_path, query, genre);
        }
        Commands::Export => {
            export_tracks(&db_path);
        }
        Commands::Stats => {
            get_stats(&music_dir, &db_path);
        }
        Commands::Search { query } => {
            search_tracks(&db_path, Some(query));
        }
        Commands::Genres => {
            list_genres(&db_path);
        }
        Commands::Compress { output_dir, format, bitrate, jobs, force, query } => {
            compress_tracks(&music_dir, &db_path, &output_dir, &format, &bitrate, jobs, force, query);
        }
        Commands::Lyrics { query, overwrite, dry_run } => {
            add_lyrics(&music_dir, &db_path, None, query, overwrite, dry_run);
        }
        Commands::Info { query } => {
            get_info(&db_path, &query);
        }
    }
}