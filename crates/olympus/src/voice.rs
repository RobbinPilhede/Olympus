use crate::pill::{self, Pill};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use olympus_core::config::home_dir;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_SHIFT};

const MODEL_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";
const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin";
const RATE: usize = 16_000;
const SHORTEST_SPEECH: Duration = Duration::from_millis(300);
// Quieter than this is an empty room, and Whisper invents polite phrases for silence.
const SILENCE_RMS: f32 = 0.002;

enum Command {
    Start { locked: bool },
    Stop,
}

/// Records while a key is held, then types what was said wherever the cursor is.
#[derive(Clone)]
pub struct Voice {
    commands: Sender<Command>,
}

impl Voice {
    /// Starts recording; locked means nothing has to stay held down.
    pub fn start(&self, locked: bool) {
        let _ = self.commands.send(Command::Start { locked });
    }

    /// Stops recording and transcribes.
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }
}

fn model_path() -> PathBuf {
    home_dir().join("models").join("whisper").join(MODEL_FILE)
}

// curl ships with Windows, and this runs once on a machine that doesn't have the model yet.
fn download_model(path: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    let Some(folder) = path.parent() else { return false };
    if std::fs::create_dir_all(folder).is_err() {
        return false;
    }
    let part = path.with_extension("part");
    let finished = std::process::Command::new("curl.exe").args(["-L", "-f", "-s", "-S", "--retry", "3", "-o"]).arg(&part).arg(MODEL_URL).creation_flags(0x0800_0000).status().is_ok_and(|status| status.success());
    finished && std::fs::rename(&part, path).is_ok()
}

fn load_engine(path: &Path) -> Option<WhisperContext> {
    let mut params = WhisperContextParameters::default();
    params.use_gpu(true);
    let engine = WhisperContext::new_with_params(&path.display().to_string(), params).ok()?;
    // The GPU compiles its shaders on the first run, which takes seconds; do that before you need it.
    let _ = transcribe(&engine, &vec![0.0; RATE]);
    Some(engine)
}

/// Encoder window for a clip, in Whisper's 20 ms steps; a fitted window is far faster than the full 30 s.
pub fn audio_window(samples: usize) -> i32 {
    let steps = samples / (RATE / 50);
    i32::try_from(steps + 64).unwrap_or(1500).min(1500)
}

/// Runs speech recognition on 16 kHz mono audio.
pub fn transcribe(engine: &WhisperContext, samples: &[f32]) -> Option<String> {
    let mut state = engine.create_state().ok()?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("auto"));
    params.set_n_threads(4);
    params.set_audio_ctx(audio_window(samples.len()));
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_print_timestamps(false);
    state.full(params, samples).ok()?;
    Some(state.as_iter().map(|segment| segment.to_string()).collect())
}

/// Removes Whisper's silence markers and spoken fillers, like `[BLANK_AUDIO]` or "um,".
pub fn clean_transcript(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut capitalize_next = false;
    for word in text.split_whitespace() {
        if (word.starts_with('[') && word.ends_with(']')) || (word.starts_with('(') && word.ends_with(')')) {
            continue;
        }
        let bare = word.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if matches!(bare.as_str(), "um" | "umm" | "uh" | "uhh" | "uhm" | "ehm" | "mm" | "hmm" | "öh" | "öhm") {
            // A filler that ended a sentence hands its full stop to the word before it.
            if word.ends_with(['.', '!', '?']) {
                if let Some(last) = out.last_mut() {
                    last.push(word.chars().last().unwrap_or('.'));
                }
            }
            capitalize_next |= out.is_empty();
            continue;
        }
        let mut word = word.to_string();
        if std::mem::take(&mut capitalize_next) {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                word = first.to_uppercase().chain(chars).collect();
            }
        }
        out.push(word);
    }
    out.join(" ")
}

/// Averages channels and resamples to Whisper's 16 kHz.
pub fn to_whisper_audio(samples: &[f32], channels: usize, rate: usize) -> Vec<f32> {
    let channels = channels.max(1);
    let mono: Vec<f32> = samples.chunks(channels).map(|frame| frame.iter().sum::<f32>() / frame.len() as f32).collect();
    if rate == RATE || mono.is_empty() {
        return mono;
    }
    let length = mono.len() * RATE / rate;
    (0..length)
        .map(|index| {
            let at = index as f64 * rate as f64 / RATE as f64;
            let low = at as usize;
            let high = (low + 1).min(mono.len() - 1);
            let fraction = (at - low as f64) as f32;
            mono[low] * (1.0 - fraction) + mono[high] * fraction
        })
        .collect()
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// The keystrokes that type some text, independent of keyboard layout.
pub fn text_inputs(text: &str) -> Vec<INPUT> {
    let mut inputs = Vec::new();
    for unit in text.encode_utf16() {
        for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
            inputs.push(INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: unit, dwFlags: flags, time: 0, dwExtraInfo: 0 } } });
        }
    }
    inputs
}

fn type_text(text: &str) {
    // A Shift still held from the chord would otherwise reach the program with the text.
    let deadline = Instant::now() + Duration::from_secs(1);
    // SAFETY: reading key state has no preconditions.
    while unsafe { GetAsyncKeyState(i32::from(VK_SHIFT.0)) } < 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let inputs = text_inputs(text);
    // SAFETY: `inputs` is a valid slice for the duration of the call.
    unsafe { SendInput(&inputs, i32::try_from(std::mem::size_of::<INPUT>()).unwrap_or(0)) };
}

struct Recording {
    _stream: cpal::Stream,
    samples: Arc<Mutex<Vec<f32>>>,
    channels: usize,
    rate: usize,
    started: Instant,
}

fn store(samples: &Mutex<Vec<f32>>, pill: &Pill, incoming: impl Iterator<Item = f32>) {
    let Ok(mut samples) = samples.lock() else { return };
    let from = samples.len();
    samples.extend(incoming);
    pill.set_level(rms(&samples[from..]));
}

fn start_recording(pill: &Pill) -> Result<Recording, String> {
    let device = cpal::default_host().default_input_device().ok_or("No microphone found")?;
    let config = device.default_input_config().map_err(|error| error.to_string())?;
    let samples = Arc::new(Mutex::new(Vec::new()));
    let (channels, rate) = (usize::from(config.channels()), config.sample_rate().0 as usize);
    let format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();
    let on_error = |_| {};
    let (buffer, level) = (samples.clone(), pill.clone());
    let stream = match format {
        cpal::SampleFormat::F32 => device.build_input_stream(&stream_config, move |data: &[f32], _: &_| store(&buffer, &level, data.iter().copied()), on_error, None),
        cpal::SampleFormat::I16 => device.build_input_stream(&stream_config, move |data: &[i16], _: &_| store(&buffer, &level, data.iter().map(|s| f32::from(*s) / 32768.0)), on_error, None),
        cpal::SampleFormat::U16 => device.build_input_stream(&stream_config, move |data: &[u16], _: &_| store(&buffer, &level, data.iter().map(|s| (f32::from(*s) - 32768.0) / 32768.0)), on_error, None),
        other => return Err(format!("Unsupported microphone format {other}")),
    }
    .map_err(|error| error.to_string())?;
    stream.play().map_err(|error| error.to_string())?;
    Ok(Recording { _stream: stream, samples, channels, rate, started: Instant::now() })
}

/// Starts the voice worker, loading the model in the background so the first hold is fast.
pub fn spawn() -> Voice {
    let (commands, inbox) = channel();
    let pill = pill::spawn();
    let engine: Arc<Mutex<Option<WhisperContext>>> = Arc::default();
    let warm = engine.clone();
    std::thread::spawn(move || {
        let path = model_path();
        if path.exists() {
            if let (Some(loaded), Ok(mut slot)) = (load_engine(&path), warm.lock()) {
                *slot = Some(loaded);
            }
        }
    });
    std::thread::spawn(move || {
        let mut recording = None;
        while let Ok(command) = inbox.recv() {
            match command {
                Command::Start { locked } => match start_recording(&pill) {
                    Ok(started) => {
                        crate::notify::tick(true);
                        pill.show_listening(locked);
                        recording = Some(started);
                    }
                    Err(message) => pill.show_message(&message),
                },
                Command::Stop => {
                    let Some(finished) = recording.take() else { continue };
                    crate::notify::tick(false);
                    finish(finished, &engine, &pill);
                }
            }
        }
    });
    Voice { commands }
}

fn finish(finished: Recording, engine: &Mutex<Option<WhisperContext>>, pill: &Pill) {
    let Recording { _stream, samples, channels, rate, started } = finished;
    drop(_stream);
    let audio = samples.lock().map(|samples| to_whisper_audio(&samples, channels, rate)).unwrap_or_default();
    if started.elapsed() < SHORTEST_SPEECH || rms(&audio) < SILENCE_RMS {
        pill.hide();
        return;
    }
    pill.show_working();
    let Ok(mut slot) = engine.lock() else { return };
    if slot.is_none() {
        let path = model_path();
        if !path.exists() {
            pill.show_downloading();
            if !download_model(&path) {
                pill.show_message("Unable to download the voice model");
                return;
            }
        }
        pill.show_working();
        *slot = load_engine(&path);
    }
    let Some(loaded) = slot.as_ref() else {
        pill.show_message("Unable to load the voice model");
        return;
    };
    let text = transcribe(loaded, &audio).map(|text| clean_transcript(&text)).unwrap_or_default();
    drop(slot);
    pill.hide();
    if !text.is_empty() {
        type_text(&text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_markers_and_fillers_disappear() {
        assert_eq!(clean_transcript(" [BLANK_AUDIO]"), "");
        assert_eq!(clean_transcript("(wind blowing)"), "(wind blowing)", "only single-word markers are dropped");
        assert_eq!(clean_transcript("So, um, add the page."), "So, add the page.");
        assert_eq!(clean_transcript("Um, add the page"), "Add the page");
        assert_eq!(clean_transcript("Add the page, uh."), "Add the page,.");
    }

    #[test]
    fn ordinary_words_that_contain_fillers_stay() {
        assert_eq!(clean_transcript("Umbrella and uhura and summer"), "Umbrella and uhura and summer");
        assert_eq!(clean_transcript("Lägg till sidan på svenska"), "Lägg till sidan på svenska");
    }

    #[test]
    fn stereo_48k_becomes_mono_16k() {
        let stereo: Vec<f32> = (0..48_000).flat_map(|_| [0.5, -0.5]).collect();
        let audio = to_whisper_audio(&stereo, 2, 48_000);
        assert_eq!(audio.len(), 16_000);
        assert!(audio.iter().all(|s| s.abs() < 0.001), "the channels were averaged");
    }

    #[test]
    fn resampling_keeps_a_ramp_in_order_and_16k_passes_through() {
        let ramp: Vec<f32> = (0..4800).map(|i| i as f32 / 4800.0).collect();
        let audio = to_whisper_audio(&ramp, 1, 48_000);
        assert_eq!(audio.len(), 1600);
        assert!(audio.windows(2).all(|pair| pair[1] >= pair[0]));
        assert_eq!(to_whisper_audio(&[0.1, 0.2], 1, 16_000), [0.1, 0.2]);
        assert!(to_whisper_audio(&[], 2, 48_000).is_empty());
    }

    #[test]
    fn the_audio_window_fits_the_clip_up_to_the_models_limit() {
        assert_eq!(audio_window(16_000 * 10), 564);
        assert_eq!(audio_window(16_000 * 60), 1500);
    }

    #[test]
    fn typed_text_presses_and_releases_every_character_including_swedish_and_emoji() {
        // SAFETY: the union member is the one these inputs were built with.
        let units: Vec<u16> = text_inputs("å😀").iter().map(|input| unsafe { input.Anonymous.ki.wScan }).collect();
        let expected: Vec<u16> = "å😀".encode_utf16().flat_map(|unit| [unit, unit]).collect();
        assert_eq!(units, expected);
    }

    #[test]
    #[ignore]
    fn real_speech_becomes_real_text_on_the_gpu() {
        let wav = std::env::var("OLYMPUS_TEST_WAV").expect("a 16 kHz mono wav");
        let bytes = std::fs::read(wav).expect("wav");
        let audio: Vec<f32> = bytes[44..].chunks_exact(2).map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0).collect();
        let engine = load_engine(&model_path()).expect("the model loads");
        let started = Instant::now();
        let text = clean_transcript(&transcribe(&engine, &audio).expect("transcribed"));
        println!("{:?}: {text}", started.elapsed());
        assert!(text.contains("settings page") && text.contains("login button"), "{text}");
        assert!(started.elapsed() < Duration::from_secs(3), "fast enough to feel instant: {:?}", started.elapsed());
    }

    #[test]
    #[ignore]
    fn the_real_microphone_delivers_audio() {
        let pill = pill::spawn();
        let recording = start_recording(&pill).expect("a microphone");
        std::thread::sleep(Duration::from_millis(600));
        let collected = recording.samples.lock().expect("samples").len();
        println!("{collected} samples, {} channels at {} Hz", recording.channels, recording.rate);
        assert!(collected > recording.rate / 10, "audio arrived");
    }
}
