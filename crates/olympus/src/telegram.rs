//! Talks to your Telegram bot: long polling needs no open port, so nothing reaches this PC from outside.
use crate::app::AppEvent;
use crate::voice::{to_whisper_audio, Voice};
use olympus_core::openai::client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::hash::{BuildHasher, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

/// Telegram refuses longer messages.
const MAX_MESSAGE: usize = 4000;
/// Telegram bots can't download larger files anyway.
const MAX_VOICE_BYTES: u64 = 20 * 1024 * 1024;
const OPUS_RATE: usize = 48_000;
const POLL_SECONDS: u64 = 50;
const TOKEN_VAR: &str = "OLYMPUS_TELEGRAM_TOKEN";
/// Pairing closes after this many wrong codes, so a stranger who finds the bot can't keep guessing.
pub const MAX_PAIRING_TRIES: u32 = 5;

/// What `telegram.json` holds. The token is a password for the bot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub token: String,
    pub chat_id: Option<i64>,
}

/// Loads the settings; the environment variable wins over the saved token.
pub fn load(path: &Path) -> Option<Settings> {
    let saved: Option<Settings> = std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok());
    match (std::env::var(TOKEN_VAR).ok().filter(|token| !token.is_empty()), saved) {
        (Some(token), saved) => Some(Settings { token, chat_id: saved.and_then(|settings| settings.chat_id) }),
        (None, saved) => saved,
    }
}

/// Saves the settings.
pub fn save(path: &Path, settings: &Settings) -> std::io::Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(settings)?)
}

/// The API address for a bot.
pub fn api_base(token: &str) -> String {
    format!("https://api.telegram.org/bot{token}")
}

/// A one-time code from the operating system's randomness, without characters that look alike.
pub fn pairing_code() -> String {
    const LETTERS: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut bits = std::collections::hash_map::RandomState::new().build_hasher().finish();
    (0..8)
        .map(|_| {
            let letter = LETTERS[(bits % LETTERS.len() as u64) as usize];
            bits /= LETTERS.len() as u64;
            char::from(letter)
        })
        .collect()
}

/// Cuts a reply into pieces Telegram accepts, at line breaks where possible.
pub fn split_message(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = text.trim();
    while rest.chars().count() > MAX_MESSAGE {
        let limit = rest.char_indices().nth(MAX_MESSAGE).map_or(rest.len(), |(at, _)| at);
        let cut = rest[..limit].rfind('\n').filter(|at| *at > 0).unwrap_or(limit);
        parts.push(rest[..cut].trim_end().to_string());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        parts.push(rest.to_string());
    }
    parts
}

#[derive(Debug, Deserialize)]
struct Reply<T> {
    ok: bool,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    chat: ChatRef,
    text: Option<String>,
    voice: Option<FileRef>,
    audio: Option<FileRef>,
}

#[derive(Debug, Deserialize)]
struct ChatRef {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Clone, Deserialize)]
struct FileRef {
    file_id: String,
    file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct FileInfo {
    file_path: String,
}

#[derive(Debug, Deserialize)]
struct BotInfo {
    username: String,
}

// Request errors contain the address, which contains the token, so they're never passed on.
async fn call<T: for<'a> Deserialize<'a>>(base: &str, method: &str, body: serde_json::Value, wait: Duration) -> Option<T> {
    let response = client().post(format!("{base}/{method}")).json(&body).timeout(wait).send().await.ok()?;
    let reply: Reply<T> = response.json().await.ok()?;
    reply.result.filter(|_| reply.ok)
}

/// The bot's @name, or None if Telegram doesn't accept the token.
pub async fn bot_name(base: &str) -> Option<String> {
    call::<BotInfo>(base, "getMe", json!({}), Duration::from_secs(15)).await.map(|bot| bot.username)
}

async fn download_voice(base: &str, file: &FileRef) -> Result<Vec<u8>, String> {
    if file.file_size.unwrap_or(0) > MAX_VOICE_BYTES {
        return Err("That recording is too long for a Telegram bot to download.".into());
    }
    let info: FileInfo = call(base, "getFile", json!({"file_id": file.file_id}), Duration::from_secs(30)).await.ok_or("Unable to fetch that recording from Telegram.")?;
    let address = base.replacen("/bot", "/file/bot", 1);
    let response = client().get(format!("{address}/{}", info.file_path)).timeout(Duration::from_secs(120)).send().await.map_err(|_| "Unable to download that recording.")?;
    let bytes = response.bytes().await.map_err(|_| "Unable to download that recording.")?;
    Ok(bytes.to_vec())
}

/// Decodes a Telegram voice memo (Opus in Ogg) to mono samples at Opus's 48 kHz.
pub fn decode_voice(bytes: &[u8]) -> Result<Vec<f32>, String> {
    const NOT_OPUS: &str = "That recording isn't in Telegram's voice format. Send a voice memo instead.";
    let mut reader = ogg::PacketReader::new(std::io::Cursor::new(bytes));
    let head = reader.read_packet().map_err(|_| NOT_OPUS)?.ok_or(NOT_OPUS)?;
    if !head.data.starts_with(b"OpusHead") || head.data.len() < 12 {
        return Err(NOT_OPUS.into());
    }
    // The encoder's warm-up samples are stored in the header, and aren't part of the recording.
    let skip = usize::from(u16::from_le_bytes([head.data[10], head.data[11]]));
    let _tags = reader.read_packet().map_err(|_| NOT_OPUS)?;
    let mut decoder = opus_decoder::OpusDecoder::new(OPUS_RATE as u32, 1).map_err(|_| "Unable to start the voice decoder.")?;
    let mut frame = vec![0.0f32; decoder.max_frame_size_per_channel()];
    let mut samples = Vec::new();
    while let Some(packet) = reader.read_packet().map_err(|_| NOT_OPUS)? {
        let count = decoder.decode_float(&packet.data, &mut frame, false).map_err(|_| "Unable to decode that recording.")?;
        samples.extend_from_slice(&frame[..count]);
    }
    samples.drain(..skip.min(samples.len()));
    Ok(samples)
}

/// What the app asks the bot to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outbound {
    Text(i64, String),
    Typing(i64),
}

/// A running connection, and the state of the chat it's linked to.
pub struct Phone {
    pub outbox: UnboundedSender<Outbound>,
    pub paired: Arc<AtomicI64>,
    pub settings_path: std::path::PathBuf,
    pub settings: Settings,
    /// The code that links a chat, until one does.
    pub code: Option<String>,
    pub wrong_codes: u32,
    /// The Olympus thread your messages go to.
    pub thread: Option<u64>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Phone {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Phone {
    /// Connects to a bot and starts listening.
    pub fn start(base: String, settings: Settings, settings_path: std::path::PathBuf, voice: Option<Voice>, events: UnboundedSender<AppEvent>) -> Self {
        let (outbox, inbox) = unbounded_channel();
        let paired = Arc::new(AtomicI64::new(settings.chat_id.unwrap_or(0)));
        let code = settings.chat_id.is_none().then(pairing_code);
        let tasks = vec![tokio::spawn(send_loop(base.clone(), inbox)), tokio::spawn(poll_loop(base, paired.clone(), voice, outbox.clone(), events))];
        Self { outbox, paired, settings_path, settings, code, wrong_codes: 0, thread: None, tasks }
    }

    /// A linked phone with no network behind it, for tests.
    #[cfg(test)]
    pub fn detached(settings_path: std::path::PathBuf, code: Option<String>) -> (Self, UnboundedReceiver<Outbound>) {
        let (outbox, inbox) = unbounded_channel();
        let paired = Arc::new(AtomicI64::new(0));
        (Self { outbox, paired, settings_path, settings: Settings { token: "t".into(), chat_id: None }, code, wrong_codes: 0, thread: None, tasks: Vec::new() }, inbox)
    }

    /// Sends text to the linked chat, if there is one.
    pub fn say(&self, text: impl Into<String>) {
        if let Some(chat) = self.settings.chat_id {
            let _ = self.outbox.send(Outbound::Text(chat, text.into()));
        }
    }

    /// Links a chat, so only it can talk to Olympus from now on.
    pub fn pair(&mut self, chat_id: i64) -> std::io::Result<()> {
        self.settings.chat_id = Some(chat_id);
        self.paired.store(chat_id, Ordering::Relaxed);
        self.code = None;
        save(&self.settings_path, &self.settings)
    }
}

async fn send_loop(base: String, mut inbox: UnboundedReceiver<Outbound>) {
    while let Some(outbound) = inbox.recv().await {
        match outbound {
            Outbound::Text(chat, text) => {
                for part in split_message(&text) {
                    let _ = call::<serde_json::Value>(&base, "sendMessage", json!({"chat_id": chat, "text": part}), Duration::from_secs(30)).await;
                }
            }
            Outbound::Typing(chat) => {
                let _ = call::<serde_json::Value>(&base, "sendChatAction", json!({"chat_id": chat, "action": "typing"}), Duration::from_secs(15)).await;
            }
        }
    }
}

async fn poll_loop(base: String, paired: Arc<AtomicI64>, voice: Option<Voice>, outbox: UnboundedSender<Outbound>, events: UnboundedSender<AppEvent>) {
    let mut next = 0;
    loop {
        let body = json!({"offset": next, "timeout": POLL_SECONDS, "allowed_updates": ["message"]});
        let Some(updates) = call::<Vec<Update>>(&base, "getUpdates", body, Duration::from_secs(POLL_SECONDS + 20)).await else {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        for update in updates {
            next = update.update_id + 1;
            let Some(message) = update.message else { continue };
            let chat = message.chat.id;
            let linked = paired.load(Ordering::Relaxed);
            // Strangers get silence, and until a chat is linked, only the code can get through.
            if message.chat.kind != "private" || (linked != 0 && chat != linked) {
                continue;
            }
            if let Some(text) = message.text {
                let _ = events.send(AppEvent::Phone { chat, text });
            } else if linked != 0 {
                match message.voice.or(message.audio) {
                    Some(file) => {
                        let (base, voice, outbox, events) = (base.clone(), voice.clone(), outbox.clone(), events.clone());
                        tokio::spawn(async move { hear(&base, chat, &file, voice, &outbox, &events).await });
                    }
                    None => {
                        let _ = outbox.send(Outbound::Text(chat, "I can read text and voice memos.".into()));
                    }
                }
            }
        }
    }
}

async fn hear(base: &str, chat: i64, file: &FileRef, voice: Option<Voice>, outbox: &UnboundedSender<Outbound>, events: &UnboundedSender<AppEvent>) {
    let _ = outbox.send(Outbound::Typing(chat));
    let heard = async {
        let voice = voice.ok_or("Voice isn't available in this Olympus.")?;
        let bytes = download_voice(base, file).await?;
        let audio = tokio::task::spawn_blocking(move || decode_voice(&bytes)).await.map_err(|_| "Unable to decode that recording.")??;
        let audio = tokio::task::spawn_blocking(move || to_whisper_audio(&audio, 1, OPUS_RATE)).await.map_err(|_| "Unable to decode that recording.")?;
        voice.transcribe_clip(audio).await
    }
    .await;
    match heard {
        Ok(text) => {
            let _ = outbox.send(Outbound::Text(chat, format!("Heard: {text}")));
            let _ = events.send(AppEvent::Phone { chat, text });
        }
        Err(problem) => {
            let _ = outbox.send(Outbound::Text(chat, problem.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn ogg_voice(frames: usize) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        {
            let mut writer = ogg::PacketWriter::new(&mut out);
            let mut head = b"OpusHead\x01\x01".to_vec();
            head.extend_from_slice(&312u16.to_le_bytes());
            head.extend_from_slice(&48_000u32.to_le_bytes());
            head.extend_from_slice(&[0, 0, 0]);
            writer.write_packet(head, 1, ogg::PacketWriteEndInfo::EndPage, 0).expect("head");
            writer.write_packet(b"OpusTags\x00\x00\x00\x00\x00\x00\x00\x00".to_vec(), 1, ogg::PacketWriteEndInfo::EndPage, 0).expect("tags");
            for index in 0..frames {
                let end = if index + 1 == frames { ogg::PacketWriteEndInfo::EndStream } else { ogg::PacketWriteEndInfo::NormalPacket };
                // The smallest valid 20 ms frame: Opus's own encoding of silence.
                writer.write_packet(vec![0xF8, 0xFF, 0xFE], 1, end, (index as u64 + 1) * 960).expect("frame");
            }
        }
        out.into_inner()
    }

    #[test]
    fn a_voice_memo_decodes_to_its_length_without_the_warm_up() {
        let samples = decode_voice(&ogg_voice(50)).expect("decode");
        assert_eq!(samples.len(), 50 * 960 - 312);
    }

    #[test]
    fn other_files_get_a_clear_refusal() {
        assert!(decode_voice(b"not audio at all").is_err());
        assert!(decode_voice(&[]).is_err());
    }

    #[test]
    fn long_replies_split_at_line_breaks_and_lose_nothing() {
        let line = "x".repeat(1500);
        let text = [line.as_str(); 5].join("\n");
        let parts = split_message(&text);
        assert!(parts.iter().all(|part| part.chars().count() <= MAX_MESSAGE));
        assert_eq!(parts.join("\n"), text);
    }

    #[test]
    fn one_long_line_and_multibyte_text_still_split() {
        let text = "å".repeat(MAX_MESSAGE * 2 + 5);
        let parts = split_message(&text);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.concat(), text);
        assert!(split_message("   ").is_empty());
    }

    #[test]
    fn pairing_codes_differ_and_are_easy_to_type() {
        let (first, second) = (pairing_code(), pairing_code());
        assert_ne!(first, second);
        assert_eq!(first.len(), 8);
        assert!(!first.contains(['O', '0', 'I', '1', 'L']));
    }

    #[test]
    fn settings_survive_a_save_and_a_missing_file_means_not_connected() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("telegram.json");
        assert_eq!(load(&path), None);
        let settings = Settings { token: "secret".into(), chat_id: Some(7) };
        save(&path, &settings).expect("save");
        assert_eq!(load(&path), Some(settings));
    }

    // One canned answer per connection, like Telegram's API.
    async fn fake_telegram(answers: Vec<&'static str>) -> (String, JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for answer in answers {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut buffer = vec![0u8; 8192];
                let read = socket.read(&mut buffer).await.expect("read");
                requests.push(String::from_utf8_lossy(&buffer[..read]).to_string());
                let response = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{answer}", answer.len());
                socket.write_all(response.as_bytes()).await.expect("write");
            }
            requests
        });
        (format!("http://{address}/botTOKEN"), server)
    }

    #[tokio::test]
    async fn the_bot_name_comes_from_telegram_and_a_bad_token_is_none() {
        let (base, server) = fake_telegram(vec![r#"{"ok":true,"result":{"username":"my_bot"}}"#, r#"{"ok":false,"description":"Unauthorized"}"#]).await;
        assert_eq!(bot_name(&base).await.as_deref(), Some("my_bot"));
        assert_eq!(bot_name(&base).await, None);
        assert!(server.await.expect("server")[0].starts_with("POST /botTOKEN/getMe"));
    }

    #[tokio::test]
    async fn a_linked_chat_is_heard_and_strangers_and_groups_are_not() {
        let updates = r#"{"ok":true,"result":[
            {"update_id":1,"message":{"chat":{"id":99,"type":"private"},"text":"let me in"}},
            {"update_id":2,"message":{"chat":{"id":7,"type":"group"},"text":"from a group"}},
            {"update_id":3,"message":{"chat":{"id":7,"type":"private"},"text":"hello"}}
        ]}"#;
        let (base, server) = fake_telegram(vec![updates]).await;
        let (events, mut heard) = unbounded_channel();
        let (outbox, _inbox) = unbounded_channel();
        let polling = tokio::spawn(poll_loop(base, Arc::new(AtomicI64::new(7)), None, outbox, events));
        let event = heard.recv().await.expect("an event");
        polling.abort();
        assert!(matches!(event, AppEvent::Phone { chat: 7, text } if text == "hello"));
        assert!(heard.try_recv().is_err());
        assert!(server.await.expect("server")[0].contains("getUpdates"));
    }

    #[tokio::test]
    async fn replies_are_sent_in_pieces_to_the_linked_chat() {
        let (base, server) = fake_telegram(vec![r#"{"ok":true,"result":{}}"#, r#"{"ok":true,"result":{}}"#]).await;
        let (outbox, inbox) = unbounded_channel();
        let sending = tokio::spawn(send_loop(base, inbox));
        outbox.send(Outbound::Text(7, "y".repeat(MAX_MESSAGE + 1))).expect("send");
        let requests = server.await.expect("server");
        sending.abort();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("sendMessage") && requests[0].contains("\"chat_id\":7"));
    }
}
