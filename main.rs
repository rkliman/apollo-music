use config as app_config;
use fs_extra::file;
use lofty::file::TaggedFileExt;
use lofty::prelude::ItemKey;
use clap::{Parser, Subcommand, ArgAction};
use serde::Deserialize;
use shellexpand;
use walkdir; // Add walkdir import
use std::fs;
use strsim;
use colored::*;
use fs_extra::dir::get_size;
use human_bytes::human_bytes;
use symphonia::core::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::default::{get_probe};
use std::fs::File;
use indicatif::{ProgressBar, ProgressStyle};


/// Search for a pattern in a file and display the lines that contain it.
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
    Ls,
    /// Export tracks to CSV
    Export,
    /// Show statistics
    Stats,
}

#[derive(Debug, Deserialize)]
struct FilesConfig {
    music_directory: String,
    database_name: String,
    file_pattern: Option<String>, // Add this line
}

#[derive(Debug, Deserialize)]
struct Settings {
    files: FilesConfig,
}

fn index_library(music_dir: &str, db_path: &str, file_pattern: Option<&str>, dry_run: bool) {
    // create or open the database

    let db_path = shellexpand::tilde(db_path).to_string();
    let mut conn = rusqlite::Connection::open(db_path).expect("Failed to open database");

    conn.execute(
        "CREATE TABLE IF NOT EXISTS tracks (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            artist TEXT,
            album TEXT,
            albumartist TEXT,
            title TEXT,
            duration INTEGER
        )",
        [],
    ).expect("Failed to create table");

    let tx = conn.transaction().expect("Failed to start transaction");

    let mut stmt = tx.prepare("SELECT path FROM tracks").expect("Failed to prepare select statement");
    let mut rows = stmt.query([]).expect("Failed to query tracks");

    let mut to_remove = Vec::new();
    while let Some(row) = rows.next().expect("Failed to fetch row") {
        let path: String = row.get(0).expect("Failed to get path");
        if !std::path::Path::new(&path).exists() {
            to_remove.push(path);
        }
    }
    drop(rows);
    drop(stmt);

    for path in to_remove {
        println!("Removing missing file from database: {}", path);
        tx.execute("DELETE FROM tracks WHERE path = ?1", [&path]).ok();
    }

    println!("Indexing music files in directory: {}", music_dir);

    // Collect all files first to know the total count
    let entries: Vec<_> = walkdir::WalkDir::new(&music_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .collect();

    let pb = ProgressBar::new(entries.len() as u64);
    pb.set_style(ProgressStyle::with_template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
        .unwrap()
        .progress_chars("##-"));

    for entry in entries {
        let path = entry.path();
        let (artist, album, albumartist, title) = match lofty::read_from_path(path) {
            Ok(tagged_file) => {
                let tag = tagged_file.primary_tag();
                let artist = tag.and_then(|t| t.get_string(&ItemKey::TrackArtist)).unwrap_or("").to_string();
                let albumartist = tag.and_then(|t| t.get_string(&ItemKey::AlbumArtist)).unwrap_or("").to_string();
                let album = tag.and_then(|t| t.get_string(&ItemKey::AlbumTitle)).unwrap_or("").to_string();
                let title = tag.and_then(|t| t.get_string(&ItemKey::TrackTitle)).unwrap_or("").to_string();
                (artist, album, albumartist, title)
            }
            Err(_) => ("".to_string(), "".to_string(), "".to_string(), "".to_string()),
        };

        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext == "mp3" || ext == "flac" || ext == "wav" {
                let mut path_str = path.to_string_lossy().to_string();

                // Move file if pattern is set
                if let Some(pattern) = file_pattern {
                    let new_rel_path = generate_path_from_pattern(
                        pattern,
                        &artist,
                        &album,
                        &title,
                        ext,
                    );
                    let new_abs_path = std::path::Path::new(music_dir).join(&new_rel_path);
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

                let result = tx.execute(
                    "INSERT OR IGNORE INTO tracks (path, artist, album, albumartist, title, duration) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    [
                        &path_str as &dyn rusqlite::ToSql,
                        &artist,
                        &albumartist,
                        &album,
                        &title,
                        &0.0 as &dyn rusqlite::ToSql,
                    ]
                );
                if let Ok(1) = result {
                    pb.set_message(format!("Added: {}", path_str));
                }
            }
        }
        pb.inc(1);
    }
    pb.finish_with_message("Indexing complete");

    tx.commit().expect("Failed to commit transaction");
}

fn find_duplicates(db_path: &str, fix: bool) {
    let db_path = shellexpand::tilde(db_path).to_string();
    let conn = rusqlite::Connection::open(db_path).expect("Failed to open database");

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
        println!("{} {}", format!("{} - {}", artist, title).cyan(), format!(" ({} times)", count).yellow());

        // Query for file paths of this duplicate track
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

        if fix && paths.len() > 1 {
            // Make "Skip" the first option
            let mut options: Vec<String> = vec!["Skip".to_string()];
            options.extend(paths.iter().map(|(_, p)| p.clone()));
            match inquire::Select::new(
                &format!("Which file do you want to keep for '{} - {}'?", artist, title),
                options.clone(),
            ).prompt() {
                Ok(selected) if selected != "Skip" => {
                    // Remove all except the selected one
                    for (id, path) in &paths {
                        if path != &selected {
                            // Delete from database
                            conn.execute("DELETE FROM tracks WHERE id = ?1", [id]).expect("Failed to delete duplicate");
                            println!("  Removed duplicate from database: {}", path);
                            // Delete from filesystem
                            match std::fs::remove_file(path) {
                                Ok(_) => println!("  Deleted file from filesystem: {}", path),
                                Err(e) => eprintln!("  Failed to delete file '{}': {}", path, e),
                            }
                        }
                    }
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

        // Map extensions to quality rank (lower is better)
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

        // If there are at least two files and the best quality is not the only one
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
    let config_path = shellexpand::tilde("~/.config/apollo-music/config.toml").to_string();
    app_config::Config::builder()
        .add_source(app_config::File::with_name(&config_path))
        .add_source(app_config::Environment::with_prefix("APP"))
        .build()
        .unwrap()
        .try_deserialize()
        .unwrap()
}

fn index_playlists(music_dir: &str, db_path: &str) {
    // loads and indexes .m3u or .m3u8 playlists in the given directory and stores them in a database
    // create or open the database
    let db_path = shellexpand::tilde(&db_path).to_string();
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

    // Remove playlists from the database that no longer exist on the filesystem
    let mut stmt = tx.prepare("SELECT path FROM playlists").expect("Failed to prepare select statement");
    let mut rows = stmt.query([]).expect("Failed to query playlists");

    let mut to_remove = Vec::new();
    while let Some(row) = rows.next().expect("Failed to fetch row") {
        let path: String = row.get(0).expect("Failed to get path");
        if !std::path::Path::new(&path).exists() {
            to_remove.push(path);
        }
    }
    drop(rows);
    drop(stmt);

    for path in to_remove {
        println!("Removing missing playlist from database: {}", path);
        tx.execute("DELETE FROM playlists WHERE path = ?1", [&path]).ok();
    }

    println!("Indexing playlists in directory: {}", music_dir);
    for entry in walkdir::WalkDir::new(&music_dir)
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

                // Check for missing files in the playlist
                if let Ok(content) = std::fs::read_to_string(path) {
                    let playlist_dir = path.parent().unwrap_or_else(|| std::path::Path::new(""));
                    for line in content.lines() {
                        let trimmed = line.trim();
                        if trimmed.is_empty() || trimmed.starts_with('#') {
                            continue;
                        }
                        // Handle relative and absolute paths
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

                            // Suggest similar files in the music directory
                            let song_file_name = song_path.file_name().and_then(|f| f.to_str()).unwrap_or("");
                            let song_name = extract_song_name_from_filename(song_file_name)
                                .unwrap_or_else(|| song_file_name.to_string());
                            println!("  Suggested song name: {}", song_name);
                            if !song_file_name.is_empty() {
                                let db_path = shellexpand::tilde(&db_path).to_string();
                                let conn = rusqlite::Connection::open(&db_path).expect("Failed to open database");
                                let mut stmt = conn.prepare("SELECT title, path FROM tracks").expect("Failed to prepare statement");
                                let mut suggestions = Vec::new();
                                let mut rows = stmt.query([]).expect("Failed to execute query");
                                while let Some(row) = rows.next().expect("Failed to fetch row") {
                                    let candidate_title: String = row.get(0).expect("Failed to get title");
                                    let candidate_path: String = row.get(1).expect("Failed to get path");
                                    let score = strsim::jaro(&candidate_title, &song_name);
                                    suggestions.push((score, candidate_path));
                                }
                                // Sort by descending similarity score and take top 5
                                suggestions.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                                let top_suggestions: Vec<_> = suggestions.into_iter().take(5).collect();
                                if !top_suggestions.is_empty() {
                                    let mut options: Vec<String> = top_suggestions
                                        .iter()
                                        .map(|(score, suggestion)| format!("({:.3}) {} ", score, suggestion))
                                        .collect();
                                    options.push("Remove".to_string());
                                    options.push("Skip".to_string());

                                    // Auto-replace if top suggestion is very similar
                                    let (top_score, top_path) = &top_suggestions[0];
                                    if *top_score >= 0.9 {
                                        println!("  Auto-replacing '{}' with '{}' (similarity {:.3})", song_path.display(), top_path, top_score);
                                        update_playlist_line(&path_str, &song_path.display().to_string(), top_path).expect("Failed to update playlist");
                                    } else {
                                        // Use inquire to let user select a replacement or skip
                                        match inquire::Select::new(
                                            &format!("Select a replacement for '{}':", song_file_name),
                                            options.clone(),
                                        ).prompt() {
                                            Ok(selected) if selected != "Skip" && selected != "Remove" => {
                                                // Extract the path from the selected option (before the space)
                                                // Extract the path from the selected option: format is "(score) path"
                                                let selected_path = selected
                                                    .splitn(2, ')')
                                                    .nth(1)
                                                    .map(|s| s.trim())
                                                    .unwrap_or(&selected);
                                                println!("  Replacing '{}' with '{}'", song_path.display(), selected_path);
                                                update_playlist_line(&path_str, &song_path.display().to_string(), selected_path).expect("Failed to update playlist");
                                            }
                                            Ok(selected) if selected == "Remove" => {
                                                // Remove the missing song from the playlist file
                                                println!("  Removing '{}' from playlist", song_path.display());
                                                // Use update_playlist_line with new_line as empty string to indicate removal
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

fn list_tracks(db_path: &str) {
    let db_path = shellexpand::tilde(db_path).to_string();
    let conn = rusqlite::Connection::open(&db_path).expect("Failed to open database");

    let mut stmt = conn.prepare("SELECT artist, album, title FROM tracks").expect("Failed to prepare statement");
    let mut rows = stmt.query([]).expect("Failed to execute query");

    println!("Track - Artist - Album");
    while let Some(row) = rows.next().expect("Failed to fetch row") {
        let artist: String = row.get(0).unwrap_or_default();
        let album: String = row.get(1).unwrap_or_default();
        let title: String = row.get(2).unwrap_or_default();
        println!(
            "{} - {} - {}",
            title,
            artist,
            album
        );
    }
}

fn export_tracks(db_path: &str) {
    let db_path = shellexpand::tilde(db_path).to_string();
    let conn = rusqlite::Connection::open(&db_path).expect("Failed to open database");

    let mut stmt = conn.prepare("SELECT artist, album, title FROM tracks").expect("Failed to prepare statement");
    let mut rows = stmt.query([]).expect("Failed to execute query");

    // Write CSV to a file in the same directory as the database, named "tracks_export.csv"
    let db_folder = std::path::Path::new(&db_path).parent().unwrap_or_else(|| std::path::Path::new("."));
    let csv_path = db_folder.join("tracks_export.csv");
    let file = std::fs::File::create(&csv_path).expect("Failed to create CSV file");
    let mut wtr = csv::Writer::from_writer(file);

    // Write CSV header
    wtr.write_record(&["Artist", "Album", "Title"]).expect("Failed to write CSV header");

    while let Some(row) = rows.next().expect("Failed to fetch row") {
        let artist: String = row.get(0).unwrap_or_default();
        let album: String = row.get(1).unwrap_or_default();
        let title: String = row.get(2).unwrap_or_default();
        wtr.write_record(&[artist, album, title]).expect("Failed to write CSV record");
    }

    wtr.flush().expect("Failed to flush CSV writer");
    println!("Exported tracks to {}", csv_path.display());
}

fn get_stats(music_dir: &str, db_path: &str) {
    let db_path = shellexpand::tilde(db_path).to_string();
    let conn = rusqlite::Connection::open(&db_path).expect("Failed to open database");

    let total_tracks: i64 = conn.query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0)).unwrap_or(0);
    let total_artists: i64 = conn.query_row("SELECT COUNT(DISTINCT artist) FROM tracks", [], |row| row.get(0)).unwrap_or(0);
    let total_albums: i64 = conn.query_row("SELECT COUNT(DISTINCT album) FROM tracks", [], |row| row.get(0)).unwrap_or(0);
    
    // update durations if they are zero
    let mut stmt = conn.prepare("SELECT id, path, duration FROM tracks WHERE duration = 0").expect("Failed to prepare statement");
    let mut rows = stmt.query([]).expect("Failed to execute query");
    while let Some(row) = rows.next().expect("Failed to fetch row") {
        let id: i64 = row.get(0).expect("Failed to get id");
        let path: String = row.get(1).expect("Failed to get path");
        let duration: f64 = get_duration_with_symphonia(std::path::Path::new(&path)) as f64;
        if duration > 0.0 {
            conn.execute("UPDATE tracks SET duration = ?1 WHERE id = ?2", [duration, id as f64]).expect("Failed to update duration");
        }
    }
    
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

    let folder_size: String = human_bytes(get_size(&music_dir).unwrap() as f64);



    println!("Total tracks: {}", total_tracks);
    println!("Total artists: {}", total_artists);
    println!("Total albums: {}", total_albums);
    println!("Total size: {}", folder_size);
    println!("Total time: {}", format_duration(total_duration));
}

fn get_duration_with_symphonia(path: &std::path::Path) -> i64 {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return 0,
    };
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let hint = Hint::new();
    let probed = match get_probe().format(&hint, mss, &Default::default(), &Default::default()) {
        Ok(p) => p,
        Err(_) => return 0,
    };
    let format = probed.format;
    let track = match format.default_track() {
        Some(t) => t,
        None => return 0,
    };
    let tb = track.codec_params.time_base;
    let dur = track.codec_params.n_frames;
    if let (Some(tb), Some(dur)) = (tb, dur) {
        tb.calc_time(dur).seconds as i64
    } else {
        0
    }
}

fn extract_song_name_from_filename(filename: &str) -> Option<String> {
    // Remove extension
    let file_stem = std::path::Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())?;
    // Split on " - " and take the second part as song name
    let parts1: Vec<&str> = file_stem.split(" - ").collect();
    let parts2: Vec<&str> = file_stem.split(" － ").collect();
    if parts1.len() > 1 {
        return Some(parts1[1].to_string());
    }
    else if parts2.len() > 1 {
        return Some(parts2[1].to_string());
        
    }
    else {
        return None;
    }
}

fn update_playlist_line(playlist_path: &str, target_line: &str, new_line: &str) -> std::io::Result<()> {
    use std::path::{Path, PathBuf};

    let content = std::fs::read_to_string(playlist_path)?;
    let playlist_dir = Path::new(playlist_path).parent().unwrap_or_else(|| Path::new(""));

    // Convert target_line and new_line to relative paths (if possible)
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
    album: &str,
    title: &str,
    ext: &str,
) -> String {
    pattern
        .replace("{artist}", artist)
        .replace("{albumartist}", artist)
        .replace("{album}", album)
        .replace("{title}", title)
        .replace("{ext}", ext)
}

fn main() {
    let settings = load_settings();

    let music_dir = shellexpand::tilde(&settings.files.music_directory).to_string();
    let db_path = shellexpand::tilde(&settings.files.database_name).to_string();
    let file_pattern = settings.files.file_pattern.as_deref();

    let db_folder = std::path::Path::new(&db_path).parent().unwrap();
    if !std::path::Path::new(&db_folder).exists() {
        fs::create_dir_all(&db_folder).expect("Failed to create music directory");
    }

    let args = Cli::parse();
    match args.command {
        Commands::Index { dry_run } => {
            index_library(&music_dir, &db_path, file_pattern, dry_run);
            index_playlists(&music_dir, &db_path);
        }
        Commands::Dupes { fix } => {
            find_duplicates(&db_path, fix);
        }
        Commands::Ls => {
            list_tracks(&db_path);
        }
        Commands::Export => {
            export_tracks(&db_path);
        }
        Commands::Stats => {
            get_stats(&music_dir, &db_path);
        }
    }
}
