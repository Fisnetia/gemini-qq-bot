//! Offline preview of the rotation seed brief for one group, rendered by
//! the exact code path a fresh conversation's first turn would use.
//! Usage: cargo run --example seed_preview -- <db-path> <bot_id> <group_id>
use gemini_qq_bot::app::turn::seed_brief_for;
use gemini_qq_bot::store::{SqliteStore, migrations::open_and_migrate};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pool = open_and_migrate(std::path::Path::new(&args[1]))
        .await
        .expect("打开 DB");
    let store = SqliteStore::new(pool);
    match seed_brief_for(&store, &args[2], &args[3]).await {
        Some((brief, targets)) => {
            println!("{brief}");
            println!("-- {} history targets --", targets.len());
            for target in &targets {
                println!(
                    "{} -> {} ({})",
                    target.history_ref, target.message_id, target.sender_id
                );
            }
        }
        None => println!("(no seed content)"),
    }
}
