use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dstreamy_mock_discord::{MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID};

#[test]
fn the_launcher_prints_where_the_mock_listens_and_exits_on_eof() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mock-discord"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let ready: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(ready["token"], MOCK_TOKEN);
    assert_eq!(ready["guild_id"], MOCK_GUILD_ID);
    assert_eq!(ready["channel_id"], MOCK_VOICE_CHANNEL_ID);
    let url = ready["gateway_url"].as_str().unwrap();
    let addr = url.strip_prefix("ws://").expect(url);
    assert!(addr.starts_with("127.0.0.1:"), "{url}");
    std::net::TcpStream::connect(addr).unwrap();

    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the launcher did not exit on EOF"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status}");
}
