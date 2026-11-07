//! Jarvis's voice: Windows' own speech, which needs no account and works offline.
use std::sync::mpsc::{channel, Sender};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Media::Speech::{ISpObjectToken, ISpObjectTokenCategory, ISpVoice, SpObjectTokenCategory, SpVoice, SPCAT_VOICES, SPF_ASYNC, SPF_IS_NOT_XML, SPF_PURGEBEFORESPEAK};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

enum Line {
    Say { text: String, lang: Option<String> },
    Stop,
}

/// Says things out loud, cutting off whatever it was saying.
#[derive(Clone)]
pub struct Speaker {
    lines: Sender<Line>,
}

impl Speaker {
    /// Starts the thread that does the talking.
    pub fn start() -> Self {
        let (lines, inbox) = channel();
        std::thread::spawn(move || {
            // SAFETY: this thread is the only one using COM here, and it lives as long as the app.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            // SAFETY: COM was started just above.
            let Ok(voice) = (unsafe { CoCreateInstance::<_, ISpVoice>(&SpVoice, None, CLSCTX_ALL) }) else { return };
            for line in inbox {
                match line {
                    Line::Say { text, lang } => say_on(&voice, &text, lang.as_deref(), false),
                    Line::Stop => stop_on(&voice),
                }
            }
        });
        Self { lines }
    }

    /// Says this, in a voice for the language if Windows has one.
    pub fn say(&self, text: &str, lang: Option<&str>) {
        let _ = self.lines.send(Line::Say { text: spoken_text(text), lang: lang.map(str::to_string) });
    }

    /// Stops talking, because you started to.
    pub fn stop(&self) {
        let _ = self.lines.send(Line::Stop);
    }
}

fn voice_for(lang: &str) -> Option<ISpObjectToken> {
    let code = match lang.to_lowercase().as_str() {
        "sv" | "swedish" => "41D",
        "en" | "english" => "409",
        _ => return None,
    };
    // SAFETY: COM is running on this thread, and the strings outlive each call.
    unsafe {
        let category: ISpObjectTokenCategory = CoCreateInstance(&SpObjectTokenCategory, None, CLSCTX_ALL).ok()?;
        category.SetId(SPCAT_VOICES, false).ok()?;
        let wanted = HSTRING::from(format!("Language={code}"));
        let tokens = category.EnumTokens(&wanted, PCWSTR::null()).ok()?;
        let mut token = None;
        tokens.Next(1, &raw mut token, None).ok()?;
        token
    }
}

fn say_on(voice: &ISpVoice, text: &str, lang: Option<&str>, wait: bool) {
    let flags = (SPF_ASYNC.0 | SPF_PURGEBEFORESPEAK.0 | SPF_IS_NOT_XML.0) as u32;
    // SAFETY: the voice and strings are live for each call, and SAPI copies the text before returning.
    unsafe {
        let _ = voice.SetVoice(lang.and_then(voice_for).as_ref());
        let _ = voice.Speak(&HSTRING::from(text), flags, None);
        if wait {
            let _ = voice.WaitUntilDone(u32::MAX);
        }
    }
}

fn stop_on(voice: &ISpVoice) {
    // SAFETY: an empty string with this flag only clears what's queued.
    let _ = unsafe { voice.Speak(&HSTRING::new(), SPF_PURGEBEFORESPEAK.0 as u32, None) };
}

/// Text without the Markdown marks a voice would read out, keeping the names inside links.
pub fn spoken_text(text: &str) -> String {
    let plain: String = text.chars().filter(|c| !"*_`#[]~".contains(*c)).collect();
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_voice_does_not_read_out_markdown() {
        assert_eq!(spoken_text("**Done.** Saved to [[Budget 2027]]\n\n# `now`"), "Done. Saved to Budget 2027 now");
    }

    #[test]
    fn windows_speech_runs_silently_in_a_test() {
        // SAFETY: COM is started for this test thread, and the voice stays on it.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let voice: ISpVoice = CoCreateInstance(&SpVoice, None, CLSCTX_ALL).expect("Windows has a speech engine");
            voice.SetVolume(0).expect("volume");
            say_on(&voice, "Testing", None, true);
            say_on(&voice, "Testing", Some("en"), true);
            say_on(&voice, "Testar", Some("sv"), true);
            say_on(&voice, "Testing", Some("klingon"), true);
            stop_on(&voice);
        }
    }
}
