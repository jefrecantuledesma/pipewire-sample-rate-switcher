use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use clap::{Arg, Command as ClapCommand};
use notify_rust::{Hint, Notification, Timeout};
use regex::Regex;

fn main() {
    let matches = ClapCommand::new("pipewire-sample-rate-switcher")
        .version("1.0")
        .about("Swap PipeWire sample rate between options listed in ~/.config/sway/config.")
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
                .help("Show parsed options and current rate; do not change anything.")
                .action(clap::ArgAction::SetTrue),
        )
        .get_matches();

    // Resolve config path
    let config_path = matches
        .get_one::<String>("config")
        .map(PathBuf::from)
        .unwrap_or_else(default_sway_config);

    // Read config
    let content = fs::read_to_string(&config_path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", config_path.display()));

    // Extract the options block
    let start_marker = "Pipewire Sample Rate Options Start";
    let end_marker = "Pipewire Sample Rate Options End";

    let lines: Vec<&str> = content.lines().collect();
    let start_idx = lines
        .iter()
        .position(|l| l.contains(start_marker))
        .unwrap_or_else(|| {
            panic!(
                "Marker '{}' not found in {}.",
                start_marker,
                config_path.display()
            )
        });
    let end_idx = lines
        .iter()
        .position(|l| l.contains(end_marker))
        .unwrap_or_else(|| {
            panic!(
                "Marker '{}' not found in {}.",
                end_marker,
                config_path.display()
            )
        });
    if end_idx <= start_idx {
        panic!("Options End marker appears before Start marker.");
    }

    // Find the line with: "# Sample Rate Options = 44100, 48000"
    let opt_line = lines[start_idx..=end_idx]
        .iter()
        .find(|l| l.trim_start().starts_with("# Sample Rate Options ="))
        .unwrap_or_else(|| {
            panic!("Could not find a line like '# Sample Rate Options = 44100, 48000' in the options block.")
        });

    // Parse integers from that line
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

    // Deduplicate & sort to make cycling deterministic
    options.sort_unstable();
    options.dedup();

    // Discover current forced rate from PipeWire
    let current = read_pw_force_rate().unwrap_or_else(|| {
        // If no forced rate is set, treat current as the first option for cycling purposes
        options[0]
    });

    if matches.get_flag("show") {
        eprintln!("Found options: {:?}.", options);
        eprintln!("Current sample rate: {}.", current);
        return;
    }

    // Pick next option (wrap around)
    let next = next_rate(&options, current);

    // Apply via pw-metadata
    match set_pw_force_rate(next) {
        Ok(_) => {
            println!(
                "Switched PipeWire clock.force-rate from {} to {}.",
                current, next
            );
            notify_ok(current, next);
        }
        Err(e) => {
            eprintln!("Failed to set PipeWire rate to {}: {}.", next, e);
            notify_err(&format!("Failed to set rate to {}: {}.", next, e));
            std::process::exit(1);
        }
    }
}

fn default_sway_config() -> PathBuf {
    let home = env::var("HOME").expect("HOME is not set.");
    PathBuf::from(home).join(".config/sway/config")
}

fn next_rate(options: &[u32], current: u32) -> u32 {
    // If current not in options, just return the first option
    if let Some(idx) = options.iter().position(|&r| r == current) {
        options[(idx + 1) % options.len()]
    } else {
        options[0]
    }
}

fn read_pw_force_rate() -> Option<u32> {
    // Command: pw-metadata -n settings 0 clock.force-rate
    // Expected stdout: a number (e.g., "48000"), or possibly "-1" / nothing if unset
    let out = Command::new("pw-metadata")
        .args(["-n", "settings", "0", "clock.force-rate"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return None;
    }
    // Sometimes output can include formatting like "clock.force-rate = 48000"
    // Extract the last number present.
    let re_num = Regex::new(r"(\d{4,5}|-1)").ok()?;
    let cap = re_num.captures(&s)?;
    let val = cap.get(1)?.as_str().parse::<i32>().ok()?;
    if val <= 0 { None } else { Some(val as u32) }
}

fn set_pw_force_rate(rate: u32) -> Result<(), String> {
    let status = Command::new("pw-metadata")
        .args(["-n", "settings", "0", "clock.force-rate", &rate.to_string()])
        .status()
        .map_err(|e| format!("Failed to execute pw-metadata: {e}"))?;

    if !status.success() {
        return Err(format!("pw-metadata exited with status {status}"));
    }
    Ok(())
}

/* ------------------------- Notifications ------------------------- */

fn notify_ok(from: u32, to: u32) {
    let _ = Notification::new()
        .summary("Pipewire Sample Rate Switcher")
        .body(&format!("Switched PipeWire rate: {} -> {} Hz.", from, to))
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
