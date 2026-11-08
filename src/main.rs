use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use clap::Parser;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::Accessor;
use rayon::prelude::*;
use serde::Serialize;
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(author, version, about = "High-speed parallel audio deduplicator and metadata scanner in static Rust")]
struct Args {
    /// Directory to scan for audio files
    #[arg(short, long, default_value = ".")]
    dir: PathBuf,

    /// Duration tolerance in seconds for fuzzy length match
    #[arg(short, long, default_value_t = 1.0)]
    tolerance: f64,

    /// Output report in JSON format
    #[arg(short, long)]
    json: bool,
}

#[derive(Debug, Clone, Serialize)]
struct AudioTrack {
    path: String,
    filename: String,
    format: String,
    duration_secs: f64,
    bitrate_kbps: u32,
    file_size_bytes: u64,
    artist: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Serialize)]
struct DuplicateGroup {
    key: String,
    tracks: Vec<AudioTrack>,
    potential_savings_bytes: u64,
}

fn is_audio_file(path: &Path) -> bool {
    match path.extension().and_then(|ext| ext.to_str()).map(|s| s.to_lowercase()) {
        Some(ext) => matches!(ext.as_str(), "mp3" | "flac" | "m4a" | "opus" | "ogg" | "wav" | "aac"),
        None => false,
    }
}

fn inspect_file(path: &Path) -> Option<AudioTrack> {
    let metadata = std::fs::metadata(path).ok()?;
    let tagged_file = Probe::open(path).ok()?.read().ok()?;

    let properties = tagged_file.properties();
    let duration_secs = properties.duration().as_secs_f64();
    let bitrate_kbps = properties.audio_bitrate().unwrap_or(0);

    let (artist, title) = if let Some(tag) = tagged_file.primary_tag() {
        (
            tag.artist().map(|s| s.to_string()),
            tag.title().map(|s| s.to_string()),
        )
    } else {
        (None, None)
    };

    let filename = path.file_name()?.to_string_lossy().to_string();
    let format = path.extension()?.to_string_lossy().to_lowercase();

    Some(AudioTrack {
        path: path.to_string_lossy().to_string(),
        filename,
        format,
        duration_secs: (duration_secs * 10.0).round() / 10.0,
        bitrate_kbps,
        file_size_bytes: metadata.len(),
        artist,
        title,
    })
}

fn normalize_str(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    let args = Args::parse();

    if !args.json {
        println!("🎵 Audio-Dedup (Static Rust) — Scanning: {}", args.dir.display());
    }

    let start_time = Instant::now();

    // 1. Discover all audio file paths (skipping hidden folders like .git, .venv)
    let file_paths: Vec<PathBuf> = WalkDir::new(&args.dir)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !name.starts_with('.') && name != "node_modules"
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && is_audio_file(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect();

    let total_found = file_paths.len();

    // 2. Parallel processing of all audio tracks using Rayon
    let tracks: Vec<AudioTrack> = file_paths
        .par_iter()
        .filter_map(|p| inspect_file(p))
        .collect();

    let scan_duration = start_time.elapsed();

    // 3. Group by Normalized (Artist + Title) or Duration bucket
    let mut groups_by_tag: HashMap<String, Vec<AudioTrack>> = HashMap::new();
    let mut groups_by_duration: HashMap<u64, Vec<AudioTrack>> = HashMap::new();

    for track in tracks {
        let tag_key = match (&track.artist, &track.title) {
            (Some(a), Some(t)) if !a.is_empty() && !t.is_empty() => {
                Some(format!("{} - {}", normalize_str(a), normalize_str(t)))
            }
            _ => None,
        };

        if let Some(key) = tag_key {
            groups_by_tag.entry(key).or_default().push(track.clone());
        }

        // Duration bucketed by tolerance (e.g. 1-second buckets)
        let dur_bucket = (track.duration_secs / args.tolerance).round() as u64;
        if dur_bucket > 10 { // ignore clips under 10 seconds
            groups_by_duration.entry(dur_bucket).or_default().push(track);
        }
    }

    // Filter duplicate groups (having 2 or more files)
    let mut duplicates: Vec<DuplicateGroup> = Vec::new();

    for (key, mut group) in groups_by_tag {
        if group.len() > 1 {
            // Sort by bitrate descending (highest quality first)
            group.sort_by(|a, b| b.bitrate_kbps.cmp(&a.bitrate_kbps));
            let total_bytes: u64 = group.iter().map(|t| t.file_size_bytes).sum();
            let keep_bytes = group[0].file_size_bytes;
            let savings = total_bytes.saturating_sub(keep_bytes);

            duplicates.push(DuplicateGroup {
                key,
                tracks: group,
                potential_savings_bytes: savings,
            });
        }
    }

    let total_savings: u64 = duplicates.iter().map(|d| d.potential_savings_bytes).sum();
    let savings_mb = total_savings as f64 / (1024.0 * 1024.0);

    if args.json {
        let report = serde_json::json!({
            "scanned_files": total_found,
            "scan_duration_ms": scan_duration.as_millis(),
            "duplicate_groups": duplicates.len(),
            "potential_savings_mb": (savings_mb * 10.0).round() / 10.0,
            "groups": duplicates,
        });
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        println!("\n✅ Beolvasva {} fájl ({:.2?})", total_found, scan_duration);
        println!("🔍 Talált duplikált zenei csoportok: {}", duplicates.len());
        println!("💾 Felszabadítható tárhely: {:.2} MB\n", savings_mb);

        for (idx, group) in duplicates.iter().take(15).enumerate() {
            println!("  [{}] „{}” ({} verzió)", idx + 1, group.key, group.tracks.len());
            for track in &group.tracks {
                println!(
                    "      • {:>5} kbps [{:>4}] {:>6.1}s {:>5.1} MB — {}",
                    track.bitrate_kbps,
                    track.format.to_uppercase(),
                    track.duration_secs,
                    track.file_size_bytes as f64 / (1024.0 * 1024.0),
                    track.filename
                );
            }
        }
        if duplicates.len() > 15 {
            println!("  ...és további {} duplikált csoport.", duplicates.len() - 15);
        }
    }
}
