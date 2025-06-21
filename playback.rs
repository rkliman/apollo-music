use mpd::{Client, song::Song};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::{time::Duration};

pub fn play(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Try to connect to MPD first
    let stream = TcpStream::connect("127.0.0.1:6600")
        .or_else(|_| {
            // If connection fails, try to start MPD
            Command::new("mpd")
                .arg("--no-daemon")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            // Wait a moment for MPD to start
            std::thread::sleep(Duration::from_millis(500));
            // Try connecting again
            TcpStream::connect("127.0.0.1:6600")
        })?;

    let mut mpd_client = Client::new(stream)?;

    println!("Playing track: {}", path);

    // Create a Song with the given path
    let mut song = Song::default();
    song.file = path.to_string();

    mpd_client.push(song)?;

    // Play music
    mpd_client.play()?;

    Ok(())
}

pub fn play_playlist(playlist_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Try to connect to MPD first
    let stream = TcpStream::connect("127.0.0.1:6600")
        .or_else(|_| {
            // If connection fails, try to start MPD
            Command::new("mpd")
                .arg("--no-daemon")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            // Wait a moment for MPD to start
            std::thread::sleep(Duration::from_millis(500));
            // Try connecting again
            TcpStream::connect("127.0.0.1:6600")
        })?;

    let mut mpd_client = Client::new(stream)?;
         

pub fn stop() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to MPD
    let stream = TcpStream::connect("127.0.0.1:6600")?;
    let mut mpd_client = Client::new(stream)?;
    // Stop playback
    mpd_client.stop()?;
    println!("Playback stopped.");
    Ok(())
}