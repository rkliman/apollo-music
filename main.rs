use config as app_config;
use inquire::length;
use lofty::file::TaggedFileExt;
use lofty::prelude::ItemKey;
use clap::{Parser, Subcommand, ArgAction};
use serde::Deserialize;
use shellexpand;
use walkdir; // Add walkdir import
use std::fs;
use strsim;
use colored::*;


/// Search for a pattern in a file and display the lines that contain it.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Index the music library and playlists
    Index,
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
}

#[derive(Debug, Deserialize)]
struct FilesConfig {
    music_directory: String,
    database_name: String,
}

#[derive(Debug, Deserialize)]
struct Settings {
    files: FilesConfig,
}

fn index_library(music_dir: &str, db_path: &str) {
    // create or open the database

    let db_path = shellexpand::tilde(db_path).to_string();
    let mut conn = rusqlite::Connection::open(db_path).expect("Failed to open database");

    conn.execute(
        "CREATE TABLE IF NOT EXISTS tracks (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            artist TEXT,
            album TEXT,
            title TEXT
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
    for entry in walkdir::WalkDir::new(&music_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        // println!("Indexing file: {:?}", path);
        let (artist, album, title) = match lofty::read_from_path(path) {
            Ok(tagged_file) => {
                let tag = tagged_file.primary_tag();
                let artist = tag.and_then(|t| t.get_string(&ItemKey::TrackArtist)).unwrap_or("").to_string();
                let album = tag.and_then(|t| t.get_string(&ItemKey::AlbumTitle)).unwrap_or("").to_string();
                let title = tag.and_then(|t| t.get_string(&ItemKey::TrackTitle)).unwrap_or("").to_string();
                (artist, album, title)
            }
            Err(_) => ("".to_string(), "".to_string(), "".to_string()),
        };
        // println!("Artist: {}, Album: {}, Title: {}", artist, album, title);

        if let Some(ext) = path.extension() {
            if ext == "mp3" || ext == "flac" || ext == "wav" {
                let path_str = path.to_string_lossy();
                let result = tx.execute(
                    "INSERT OR IGNORE INTO tracks (path, artist, album, title) VALUES (?1, ?2, ?3, ?4)",
                    [
                        &path_str as &dyn rusqlite::ToSql,
                        &artist,
                        &album,
                        &title,
                    ]
                );
                if let Ok(1) = result {
                    println!("Added to database: {}", path_str);
                }
            }
        }
    }

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

    while let Some(row) = rows.next().expect("Failed to fetch row") {
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

    // Identify tracks where a lower quality version exists (FLAC > M4A > MP3)
    println!("\nTracks with lower quality duplicates (FLAC > M4A > MP3):");

    let mut stmt = conn.prepare(
        "SELECT artist, title, GROUP_CONCAT(path) as paths FROM tracks \
         WHERE artist != '' AND title != '' \
         GROUP BY artist, title HAVING COUNT(*) > 1"
    ).expect("Failed to prepare statement for quality check");

    let mut rows = stmt.query([]).expect("Failed to execute quality check query");

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
                            if !song_file_name.is_empty() {
                                let db_path = shellexpand::tilde(&db_path).to_string();
                                let conn = rusqlite::Connection::open(&db_path).expect("Failed to open database");
                                let mut stmt = conn.prepare("SELECT path FROM tracks").expect("Failed to prepare statement");
                                let mut suggestions = Vec::new();
                                let mut rows = stmt.query([]).expect("Failed to execute query");
                                while let Some(row) = rows.next().expect("Failed to fetch row") {
                                    let candidate_path: String = row.get(0).expect("Failed to get path");
                                    let candidate_file_name = std::path::Path::new(&candidate_path)
                                        .file_name()
                                        .and_then(|f| f.to_str())
                                        .unwrap_or("");
                                    let score = strsim::jaro(candidate_file_name, song_file_name);
                                    suggestions.push((score, candidate_path));
                                }
                                // Sort by descending similarity score and take top 5
                                suggestions.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                                let top_suggestions: Vec<_> = suggestions.into_iter().take(5).collect();
                                if !top_suggestions.is_empty() {
                                    println!("  Top suggestions for '{}':", song_file_name);
                                    let mut options: Vec<String> = top_suggestions
                                        .iter()
                                        .map(|(_, suggestion)| suggestion.clone())
                                        .collect();
                                    options.push("Skip".to_string());

                                    // Use inquire to let user select a replacement or skip
                                    match inquire::Select::new(
                                        &format!("Select a replacement for '{}':", song_file_name),
                                        options.clone(),
                                    ).prompt() {
                                        Ok(selected) if selected != "Skip" => {
                                            // Replace the missing song in the playlist file
                                            println!("  Replacing '{}' with '{}'", song_path.display(), selected);
                                            let new_content: String = content.lines()
                                                .map(|line| {
                                                    if line.trim() == trimmed {
                                                        selected.clone()
                                                    } else {
                                                        line.to_string()
                                                    }
                                                })
                                                .collect::<Vec<_>>()
                                                .join("\n");
                                            if let Err(e) = std::fs::write(path, new_content) {
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
    tx.commit().expect("Failed to commit transaction");
}

fn list_tracks(db_path: &str) {
    let db_path = shellexpand::tilde(db_path).to_string();
    let conn = rusqlite::Connection::open(&db_path).expect("Failed to open database");

    let mut stmt = conn.prepare("SELECT artist, album, title FROM tracks").expect("Failed to prepare statement");
    let mut rows = stmt.query([]).expect("Failed to execute query");

    while let Some(row) = rows.next().expect("Failed to fetch row") {
        let artist: String = row.get(0).unwrap_or_default();
        let album: String = row.get(1).unwrap_or_default();
        let title: String = row.get(2).unwrap_or_default();
        println!(
            "Track: {}, Artist: {}, Album: {}",
            title.cyan(),
            artist.cyan(),
            album.cyan()
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

fn main() {
    let settings = load_settings();

    let music_dir = shellexpand::tilde(&settings.files.music_directory).to_string();
    let db_path = shellexpand::tilde(&settings.files.database_name).to_string();

    let db_folder = std::path::Path::new(&db_path).parent().unwrap();
    if !std::path::Path::new(&db_folder).exists() {
        fs::create_dir_all(&db_folder).expect("Failed to create music directory");
    }

    let args = Cli::parse();
    match args.command {
        Commands::Index => {
            index_library(&music_dir, &db_path);
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
    }
}
