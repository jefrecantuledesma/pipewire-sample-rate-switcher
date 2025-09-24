use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{Arg, Command as ClapCommand};
use notify_rust::{Hint, Notification, Timeout};
use regex::Regex;

fn main() {
    let matches = ClapCommand::new("pipewire-sample-rate-switcher")
        .version("1.2")
        .about("Swap PipeWire sample rate by editing ~/.config/pipewire/pipewire.conf.d/99-samplerate.conf and restarting PipeWire.")
        .arg(
            Arg::new("config")
                .long("config")
                .value_name("PATH")
                .help("Path to sway config (default: ~/.config/sway/config).")
                .required(false),
        )
        .arg(
            Arg::new("show")
                .long("show")
                .help("Show parsed options and current rate (file & graph if available); do not change anything.")
                .action(clap::ArgAction::SetTrue),
        )
        .get_matches();

    // Resolve paths
    let sway_config = matches
        .get_one::<String>("config")
        .map(PathBuf::from)
        .unwrap_or_else(default_sway_config);

    let samplerate_conf = default_samplerate_conf();

    // Parse options from Sway config block
    let content = fs::read_to_string(&sway_config)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", sway_config.display()));

    let options = parse_options_from_sway(
        &content,
        "Pipewire Sample Rate Options Start",
        "Pipewire Sample Rate Options End",
    );

    // Current (from file), fall back to first option if unknown
    let current_file = read_rate_from_file(&samplerate_conf).unwrap_or(options[0]);

    if matches.get_flag("show") {
        eprintln!("Sway options: {:?}.", options);
        eprintln!("Current file rate: {}.", current_file);
        if let Some(gr) = read_graph_rate_quick() {
            eprintln!("(Live) graph rate: {}.", gr);
        }
        return;
    }

    // Compute next
    let next = next_rate(&options, current_file);

    // Overwrite 99-samplerate.conf with a canonical block (no weird formatting)
    if let Err(e) = write_canonical_samplerate_conf(&samplerate_conf, next, &options) {
        notify_err(&format!(
            "Failed to update {}: {e}.",
            samplerate_conf.display()
        ));
        eprintln!("Failed to update {}: {e}.", samplerate_conf.display());
        std::process::exit(1);
    }

    // Restart PW stack
    if let Err(e) = restart_pipewire_stack() {
        notify_err(&format!("Updated file, but restart failed: {e}."));
        eprintln!("Updated file, but restart failed: {e}.");
        std::process::exit(1);
    }

    println!("Switched default.clock.rate: {} -> {}", current_file, next);
    notify_ok(current_file, next);
}

/* ------------------------- Paths ------------------------- */

fn default_sway_config() -> PathBuf {
    PathBuf::from(env::var("HOME").expect("HOME not set")).join(".config/sway/config")
}

fn default_samplerate_conf() -> PathBuf {
    PathBuf::from(env::var("HOME").expect("HOME not set"))
        .join(".config/pipewire/pipewire.conf.d/99-samplerate.conf")
}

/* ------------------------- Parsing ------------------------- */

fn parse_options_from_sway(content: &str, start_marker: &str, end_marker: &str) -> Vec<u32> {
    let lines: Vec<&str> = content.lines().collect();
    let start_idx = lines
        .iter()
        .position(|l| l.contains(start_marker))
        .unwrap_or_else(|| panic!("Marker '{}' not found in sway config.", start_marker));
    let end_idx = lines
        .iter()
        .position(|l| l.contains(end_marker))
        .unwrap_or_else(|| panic!("Marker '{}' not found in sway config.", end_marker));
    if end_idx <= start_idx {
        panic!("Options End marker appears before Start marker.");
    }

    // Find: "# Sample Rate Options = 44100, 48000"
    let opt_line = lines[start_idx..=end_idx]
        .iter()
        .find(|l| l.trim_start().starts_with("# Sample Rate Options ="))
        .unwrap_or_else(|| {
            panic!("Could not find a line like '# Sample Rate Options = 44100, 48000' in the options block.");
        });

    let re_num = Regex::new(r"(\d{4,5})").unwrap();
    let mut options: Vec<u32> = re_num
        .captures_iter(opt_line)
        .filter_map(|c| c.get(1).and_then(|m| m.as_str().parse::<u32>().ok()))
        .collect();

    if options.is_empty() {
        panic!(
            "No sample-rate numbers found on options line: {}.",
            opt_line
        );
    }
    options.sort_unstable();
    options.dedup();
    options
}

fn next_rate(options: &[u32], current: u32) -> u32 {
    if let Some(i) = options.iter().position(|&r| r == current) {
        options[(i + 1) % options.len()]
    } else {
        options[0]
    }
}

/* ------------------------- File read/write ------------------------- */

fn read_rate_from_file(path: &Path) -> Option<u32> {
    let s = fs::read_to_string(path).ok()?;
    // Looser match: find rate anywhere, even if it's on the same line as the '{'
    let re = Regex::new(r#"default\.clock\.rate\s*=\s*"?(\d{4,5})"?"#).ok()?;
    let caps = re.captures(&s)?;
    caps.get(1)?.as_str().parse::<u32>().ok()
}

fn write_canonical_samplerate_conf(
    path: &Path,
    new_rate: u32,
    allowed_all: &[u32],
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut v = allowed_all.to_vec();
    v.sort_unstable();
    v.dedup();
    let allowed_bracket = format!(
        "[ {} ]",
        v.iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );

    let text = format!(
        "context.properties = {{\n    default.clock.rate          = {}\n    default.clock.allowed-rates = {}\n}}\n",
        new_rate, allowed_bracket
    );

    fs::write(path, text)
}

/* ------------------------- Restart helpers ------------------------- */

fn restart_pipewire_stack() -> Result<(), String> {
    // Try a straight restart first
    let status = Command::new("systemctl")
        .args([
            "--user",
            "restart",
            "pipewire.service",
            "pipewire-pulse.service",
            "wireplumber.service",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to exec systemctl: {e}"))?;

    if status.success() {
        return Ok(());
    }

    // Fallback: stop socket, then start services and socket again
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "pipewire.socket"])
        .status();

    let ok_pw = Command::new("systemctl")
        .args(["--user", "start", "pipewire.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let _ = Command::new("systemctl")
        .args(["--user", "start", "pipewire.socket"])
        .status();

    let ok_wp = Command::new("systemctl")
        .args(["--user", "restart", "wireplumber.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if ok_pw && ok_wp {
        Ok(())
    } else {
        Err("PipeWire/WirePlumber restart failed (even after socket bounce).".into())
    }
}

/* ------------------------- Optional: read current graph rate (info only) ------------------------- */

fn read_graph_rate_quick() -> Option<u32> {
    let out = Command::new("pw-metadata")
        .args(["-n", "settings", "0", "clock.rate"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let re = Regex::new(r"(\d{4,5})").ok()?;
    re.captures(&s)?.get(1)?.as_str().parse::<u32>().ok()
}

/* ------------------------- Notifications ------------------------- */

fn notify_ok(from: u32, to: u32) {
    let _ = Notification::new()
        .summary("Pipewire Sample Rate Switcher")
        .body(&format!(
            "Switched default.clock.rate: {} -> {} Hz.",
            from, to
        ))
        .icon("audio-card")
        .appname("pipewire-sample-rate-switcher")
        .hint(Hint::Category("Device".to_owned()))
        .timeout(Timeout::Milliseconds(6000))
        .show();
}

fn notify_err(msg: &str) {
    let _ = Notification::new()
        .summary("Pipewire Sample Rate Switcher — Error")
        .body(msg)
        .icon("dialog-error")
        .appname("pipewire-sample-rate-switcher")
        .timeout(Timeout::Milliseconds(8000))
        .show();
}
