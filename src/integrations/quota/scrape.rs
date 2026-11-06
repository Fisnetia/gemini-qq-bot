//! Driving an interactive agy in a scratch HOME to scrape the /usage page.
//! Moved out of quota.rs verbatim (std-split-quota); logic unchanged.

use super::*;

/// Drive an interactive `agy` in a scratch HOME over a PTY, open `/usage`,
/// and parse both Gemini buckets from the rendered page.
///
/// Returns the parsed buckets and the observation time. Runs on a blocking
/// thread: all I/O here is synchronous.
#[cfg(unix)]
pub(super) fn scrape_usage(
    agy_home: &Path,
    agy_bin: &Path,
) -> Result<(UsageBuckets, i64), &'static str> {
    use std::os::unix::process::CommandExt;

    let scratch = prepare_scratch_home(agy_home)?;
    let command = format!(
        "stty rows 50 cols 140 2>/dev/null; exec {}",
        shell_quote(&agy_bin.to_string_lossy())
    );
    let mut child = Command::new("script")
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .current_dir(&scratch)
        .env("HOME", &scratch)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| "tui_spawn")?;
    let mut stdin = child.stdin.take().ok_or("tui_stdin")?;
    let mut stdout = child.stdout.take().ok_or("tui_stdout")?;
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let reader = {
        let buffer = Arc::clone(&buffer);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 16384];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut buf = buffer.lock().unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.len() > 1_048_576 {
                            let excess = buf.len() - 1_048_576;
                            buf.drain(..excess);
                        }
                    }
                }
            }
        })
    };
    let rendered = |buffer: &Arc<Mutex<Vec<u8>>>| -> String {
        let bytes = buffer.lock().unwrap().clone();
        strip_ansi(&String::from_utf8_lossy(&bytes))
    };

    let deadline = Instant::now() + SCRAPE_TIMEOUT;
    let mut trust_sent = false;
    let mut usage_sent = false;
    let mut page_seen_at: Option<Instant> = None;
    let result = loop {
        if Instant::now() > deadline {
            break Err("tui_timeout");
        }
        let text = rendered(&buffer);
        if !trust_sent && text.contains("Do you trust the contents of this project?") {
            let _ = stdin.write_all(b"\r");
            let _ = stdin.flush();
            trust_sent = true;
        } else if trust_sent && !usage_sent && text.contains("? for shortcuts") {
            std::thread::sleep(Duration::from_millis(700));
            let _ = stdin.write_all(b"/usage");
            let _ = stdin.flush();
            std::thread::sleep(Duration::from_millis(800));
            let _ = stdin.write_all(b"\r");
            let _ = stdin.flush();
            usage_sent = true;
        } else if usage_sent && text.contains("Weekly Limit") && text.contains("Five Hour Limit") {
            // Let the page finish rendering, then take the final frame.
            if page_seen_at.is_none() {
                page_seen_at = Some(Instant::now());
            }
            if page_seen_at
                .map(|t| t.elapsed() > Duration::from_millis(2500))
                .unwrap_or(false)
            {
                let buckets = parse_usage(&text);
                if buckets.weekly.is_some() || buckets.five_hour.is_some() {
                    break Ok((buckets, crate::now_unix()));
                }
                break Err("tui_parse");
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    // Terminate the session whatever happened; the scratch CLI must never
    // outlive the scrape.
    let pid = child.id();
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    result
}

/// The scrape drives agy through the unix `script` PTY wrapper and a
/// POSIX shell; there is no equivalent on Windows, so the quota probe
/// reports an error there and pacing runs on its fallback path.
#[cfg(not(unix))]
pub(super) fn scrape_usage(
    _agy_home: &Path,
    _agy_bin: &Path,
) -> Result<(UsageBuckets, i64), &'static str> {
    Err("tui_unsupported_platform")
}

/// (Re)build the scratch HOME: a minimal Agy home whose token copy is
/// refreshed from the real home, so a scrape always starts from the real
/// home's current credentials without ever writing to it. Onboarding and
/// trust state accumulated in the scratch home itself is left in place.
pub(super) fn prepare_scratch_home(agy_home: &Path) -> Result<PathBuf, &'static str> {
    let parent = agy_home.parent().ok_or("tui_home")?;
    let scratch = parent.join("quota-tui-home");
    let cli_dir = scratch.join(".gemini/antigravity-cli");
    std::fs::create_dir_all(cli_dir.join("cache")).map_err(|_| "tui_mkdir")?;
    const FILES: [&str; 5] = [
        ".gemini/antigravity-cli/antigravity-oauth-token",
        ".gemini/antigravity-cli/settings.json",
        ".gemini/antigravity-cli/installation_id",
        ".gemini/antigravity-cli/jetski_state.pbtxt",
        ".gemini/antigravity-cli/cache/onboarding.json",
    ];
    let mut copied_token = false;
    for rel in FILES {
        let src = agy_home.join(rel);
        if !src.is_file() {
            continue;
        }
        let dst = scratch.join(rel);
        std::fs::copy(&src, &dst).map_err(|_| "tui_copy")?;
        if rel.ends_with("antigravity-oauth-token") {
            copied_token = true;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    if !copied_token {
        return Err("tui_token");
    }
    Ok(scratch)
}

pub(super) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
