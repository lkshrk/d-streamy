//! Serves the mock until stdin closes, for tests outside Rust; the first stdout line says how to reach it.

use std::io::Write;

use dstreamy_mock_discord::{MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mock = MockDiscord::start().await?;
    let ready = serde_json::json!({
        "gateway_url": mock.gateway_url(),
        "token": MOCK_TOKEN,
        "guild_id": MOCK_GUILD_ID,
        "channel_id": MOCK_VOICE_CHANNEL_ID,
    });
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{ready}")?;
    stdout.flush()?;
    drop(stdout);
    let (eof_tx, eof) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
        let _ = eof_tx.send(());
    });
    let _ = eof.await;
    mock.shutdown().await;
    Ok(())
}
