//! Live smoke test for the agy aux backend (stage 2 dual-mode).
//!
//! Ignored by default: it spawns a real Agy session against a logged-in
//! Agy home and spends a small amount of real model quota. Run it
//! manually on a machine with a working `agy` login:
//!
//! ```sh
//! AUX_SMOKE_AGY_BIN=/path/to/agy AUX_SMOKE_AGY_HOME=$HOME \
//!   cargo test --test aux_smoke -- --ignored --nocapture
//! ```
//!
//! `AUX_SMOKE_AGY_HOME` follows the bot's own convention: it is the
//! `$HOME` directory that *contains* `.gemini/`, not the `.gemini`
//! directory itself.
//!
//! It verifies the whole aux-session path — spawn, delivery, step-stream
//! settle detection, text extraction, and image staging via `@path` —
//! that the unit tests can only fake.

use std::path::PathBuf;
use std::time::Duration;

use gemini_qq_bot::integrations::agy_aux::{AgyAuxClient, AgyAuxConfig};
use gemini_qq_bot::integrations::aux_llm::AuxLlm;

/// One solid-red 16x16 PNG.
const RED_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAIAAACQkWg2AAAAF0lEQVR4nGP4z8BAEiJN9aiGUQ1DSgMAkPn/Afnh+ngAAAAASUVORK5CYII=";

fn smoke_config(workspace: PathBuf) -> AgyAuxConfig {
    let home = std::env::var("AUX_SMOKE_AGY_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").expect("HOME set")));
    AgyAuxConfig {
        bin: PathBuf::from(std::env::var("AUX_SMOKE_AGY_BIN").unwrap_or_else(|_| "agy".into())),
        home,
        workspace,
        model: std::env::var("AUX_SMOKE_MODEL").unwrap_or_else(|_| "gemini-3.8-flash".into()),
        turn_limit: 35,
    }
}

#[tokio::test]
#[ignore = "live smoke: needs a logged-in agy home, spends real quota"]
async fn agy_aux_answers_text_and_vision() {
    let dir = tempfile::tempdir().expect("tmp");
    let client = AgyAuxClient::new(smoke_config(dir.path().join("aux-ws")));

    let text = tokio::time::timeout(
        Duration::from_secs(300),
        client.complete_streaming(
            "You answer utility questions for another program, briefly and exactly.",
            "Reply with exactly this token and nothing else: AUX-OK",
            None,
        ),
    )
    .await
    .expect("text call within 300s")
    .expect("text call succeeds");
    println!("text answer: {text}");
    assert!(
        text.contains("AUX-OK"),
        "answer should echo the token: {text}"
    );

    let data_url = format!("data:image/png;base64,{RED_PNG_B64}");
    let vision = tokio::time::timeout(
        Duration::from_secs(300),
        client.complete_vision_on(
            "any",
            "You answer utility questions for another program, briefly and exactly.",
            "What single colour fills the image? Answer with the colour word only.",
            &[data_url],
            None,
        ),
    )
    .await
    .expect("vision call within 300s")
    .expect("vision call succeeds");
    println!("vision answer: {vision}");
    assert!(
        vision.to_lowercase().contains("red"),
        "answer should name the colour: {vision}"
    );
}
