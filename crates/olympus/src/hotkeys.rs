use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::w;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, DispatchMessageW, GetMessageW, KillTimer, SetTimer, SetWindowsHookExW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER};

/// How long the key must stay down before it counts as a hold instead of typing a capital.
const HOLD_MS: u32 = 250;
/// How long after a tap a second tap still counts as a double tap.
const DOUBLE_TAP_MS: u32 = 300;
const VK_V: u16 = 0x56;
const VK_C: u16 = 0x43;
const VK_B: u16 = 0x42;
// Marks our own replays; other injected keys are real typing from tools like Parsec.
const OWN_INPUT: usize = 0x4F4C_594D;

static ENABLED: AtomicBool = AtomicBool::new(true);

/// Turns the chords on or off; when off, every key passes straight through.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// What a chord does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chord {
    Voice,
    Shot,
    Jarvis,
}

/// A chord starting or ending, sent to whoever handles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Start(Chord),
    /// Listening that keeps running without holding anything, until the chord is pressed again.
    StartLocked(Chord),
    Stop(Chord),
}

/// What the machine wants done about one key event.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Decision {
    /// Hide this event from every other program.
    pub swallow: bool,
    /// Type this key now, since it turned out to be an ordinary capital.
    pub replay: Option<u16>,
    /// Send this event on after the replay, so typed letters keep their order.
    pub reinject: bool,
    /// Start the timer with this many milliseconds, replacing any running one.
    pub arm_timer: Option<u32>,
    pub disarm_timer: bool,
    pub signal: Option<Signal>,
}

/// What a finished timer means.
#[derive(Debug, PartialEq, Eq)]
pub enum Expired {
    Nothing,
    Signal(Signal),
    /// No second tap came, so the first was a capital after all.
    Replay(u16),
}

/// Modifiers held during a key event.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mods {
    pub shift: bool,
    /// Ctrl, Alt or Windows; those combinations belong to other programs, like Ctrl+Shift+V.
    pub other: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// The key is down; a long press becomes a hold.
    Press(u16),
    /// The key tapped once; a second tap makes a double tap.
    Second(u16),
}

/// Tells a tap (types a capital) from a hold or a double tap (runs the chord).
#[derive(Debug, Default)]
pub struct Machine {
    waiting: Option<Wait>,
    held: Option<(u16, Chord)>,
    locked: Option<u16>,
    // After a chord the key may still be down, and must not start typing lowercase.
    draining: Option<u16>,
}

fn chord_of(vk: u16) -> Option<Chord> {
    match vk {
        VK_V => Some(Chord::Voice),
        VK_C => Some(Chord::Shot),
        VK_B => Some(Chord::Jarvis),
        _ => None,
    }
}

fn is_shift(vk: u16) -> bool {
    matches!(vk, 0x10 | 0xA0 | 0xA1)
}

impl Machine {
    /// Handles one physical key event.
    pub fn key(&mut self, vk: u16, down: bool, mods: Mods) -> Decision {
        let swallowed = Decision { swallow: true, ..Decision::default() };
        if self.draining == Some(vk) {
            if !down {
                self.draining = None;
            }
            return swallowed;
        }
        let chord_press = down && mods.shift && !mods.other;
        match self.waiting {
            Some(Wait::Press(pending)) if vk == pending => {
                if down {
                    return swallowed;
                }
                self.waiting = Some(Wait::Second(pending));
                return Decision { swallow: true, arm_timer: Some(DOUBLE_TAP_MS), ..Decision::default() };
            }
            Some(Wait::Second(pending)) if vk == pending && chord_press => {
                self.waiting = None;
                self.draining = Some(vk);
                let signal = match chord_of(vk) {
                    Some(chord) if chord != Chord::Shot => {
                        self.locked = Some(vk);
                        Signal::StartLocked(chord)
                    }
                    _ => Signal::Start(Chord::Shot),
                };
                return Decision { swallow: true, disarm_timer: true, signal: Some(signal), ..Decision::default() };
            }
            // Only a new key means you've moved on: a letter you just typed being released doesn't.
            Some(Wait::Press(pending) | Wait::Second(pending)) if down && !is_shift(vk) => {
                self.waiting = None;
                return Decision { swallow: true, replay: Some(pending), reinject: true, disarm_timer: true, ..Decision::default() };
            }
            Some(Wait::Press(pending)) if is_shift(vk) && !down => {
                self.waiting = None;
                return Decision { swallow: true, replay: Some(pending), reinject: true, disarm_timer: true, ..Decision::default() };
            }
            Some(_) => return Decision::default(),
            None => {}
        }
        if let Some((key, chord)) = self.held {
            if vk == key {
                if down {
                    return swallowed;
                }
                self.held = None;
                return Decision { swallow: true, signal: Some(Signal::Stop(chord)), ..Decision::default() };
            }
            if is_shift(vk) && !down {
                self.held = None;
                self.draining = Some(key);
                return Decision { signal: Some(Signal::Stop(chord)), ..Decision::default() };
            }
            return Decision::default();
        }
        if let Some(chord) = chord_of(vk).filter(|_| self.locked == Some(vk) && chord_press) {
            self.locked = None;
            self.draining = Some(vk);
            return Decision { swallow: true, signal: Some(Signal::Stop(chord)), ..Decision::default() };
        }
        if chord_press && chord_of(vk).is_some() {
            self.waiting = Some(Wait::Press(vk));
            return Decision { swallow: true, arm_timer: Some(HOLD_MS), ..Decision::default() };
        }
        Decision::default()
    }

    /// The running timer ran out.
    pub fn timer(&mut self) -> Expired {
        match self.waiting.take() {
            Some(Wait::Press(vk)) => match chord_of(vk) {
                Some(chord) => {
                    self.held = Some((vk, chord));
                    Expired::Signal(Signal::Start(chord))
                }
                None => Expired::Nothing,
            },
            Some(Wait::Second(vk)) => Expired::Replay(vk),
            None => Expired::Nothing,
        }
    }
}

struct Hook {
    machine: Machine,
    timer: usize,
    on_signal: Box<dyn Fn(Signal)>,
}

thread_local! {
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

fn key_input(vk: u16, up: bool, extended: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: OWN_INPUT } } }
}

/// The keystrokes for a capital, adding Shift when you've already let go of it.
pub fn capital_inputs(letter: u16, shift_held: bool) -> Vec<INPUT> {
    let mut inputs = Vec::new();
    if !shift_held {
        inputs.push(key_input(VK_SHIFT.0, false, false));
    }
    inputs.push(key_input(letter, false, false));
    inputs.push(key_input(letter, true, false));
    if !shift_held {
        inputs.push(key_input(VK_SHIFT.0, true, false));
    }
    inputs
}

fn send(inputs: &[INPUT]) {
    if !inputs.is_empty() {
        // SAFETY: `inputs` is a valid slice for the duration of the call.
        unsafe { SendInput(inputs, i32::try_from(std::mem::size_of::<INPUT>()).unwrap_or(0)) };
    }
}

fn held(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: reading key state has no preconditions.
    unsafe { GetAsyncKeyState(i32::from(vk.0)) < 0 }
}

fn kill_timer(hook: &mut Hook) {
    if hook.timer != 0 {
        // SAFETY: the timer belongs to this thread.
        let _ = unsafe { KillTimer(None, hook.timer) };
        hook.timer = 0;
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for WH_KEYBOARD_LL, lparam always points at a KBDLLHOOKSTRUCT for this call.
        let info = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let message = wparam.0 as u32;
        let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
        let known = down || message == WM_KEYUP || message == WM_SYSKEYUP;
        let own = (info.flags.0 & LLKHF_INJECTED.0) != 0 && info.dwExtraInfo == OWN_INPUT;
        if known && !own && ENABLED.load(Ordering::Relaxed) {
            let vk = info.vkCode as u16;
            let shift = held(VK_SHIFT);
            let mods = Mods { shift, other: held(VK_CONTROL) || held(VK_MENU) || held(VK_LWIN) || held(VK_RWIN) };
            let swallow = HOOK.with(|hook| {
                let mut hook = hook.borrow_mut();
                let Some(hook) = hook.as_mut() else { return false };
                let decision = hook.machine.key(vk, down, mods);
                let mut inputs = decision.replay.map_or_else(Vec::new, |letter| capital_inputs(letter, shift));
                if decision.reinject {
                    inputs.push(key_input(vk, !down, (info.flags.0 & LLKHF_EXTENDED.0) != 0));
                }
                send(&inputs);
                if decision.disarm_timer || decision.arm_timer.is_some() {
                    kill_timer(hook);
                }
                if let Some(milliseconds) = decision.arm_timer {
                    // SAFETY: a thread timer needs no window, and this thread pumps messages.
                    hook.timer = unsafe { SetTimer(None, 0, milliseconds, None) };
                }
                if let Some(signal) = decision.signal {
                    (hook.on_signal)(signal);
                }
                decision.swallow
            });
            if swallow {
                return LRESULT(1);
            }
        }
    }
    // SAFETY: passing the event on is what every hook must do for events it keeps.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Watches the keyboard everywhere on its own thread for as long as Olympus runs.
pub fn spawn(on_signal: impl Fn(Signal) + Send + 'static) {
    std::thread::spawn(move || {
        // A second Olympus waits its turn, or one hold would record and type twice.
        // SAFETY: a named mutex with default security, kept until this process exits.
        let Ok(turn) = (unsafe { CreateMutexW(None, false, w!("OlympusHotkeys")) }) else { return };
        // SAFETY: the handle stays open; a previous owner exiting also ends this wait.
        unsafe { WaitForSingleObject(turn, INFINITE) };
        HOOK.with(|hook| *hook.borrow_mut() = Some(Hook { machine: Machine::default(), timer: 0, on_signal: Box::new(on_signal) }));
        // SAFETY: the callback lives for the whole program and this thread pumps messages below.
        if unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) }.is_err() {
            return;
        }
        let mut message = MSG::default();
        // SAFETY: `message` outlives each call; no window means this thread's own queue.
        while unsafe { GetMessageW(&raw mut message, None, 0, 0) }.as_bool() {
            if message.message == WM_TIMER {
                HOOK.with(|hook| {
                    let mut hook = hook.borrow_mut();
                    let Some(hook) = hook.as_mut() else { return };
                    kill_timer(hook);
                    match hook.machine.timer() {
                        Expired::Signal(signal) => (hook.on_signal)(signal),
                        Expired::Replay(letter) => send(&capital_inputs(letter, held(VK_SHIFT))),
                        Expired::Nothing => {}
                    }
                });
            }
            // SAFETY: the message came from GetMessageW just above.
            unsafe { DispatchMessageW(&raw const message) };
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIFT: Mods = Mods { shift: true, other: false };
    const NONE: Mods = Mods { shift: false, other: false };
    const VK_A: u16 = 0x41;
    const VK_LSHIFT: u16 = 0xA0;

    fn tap(m: &mut Machine, vk: u16) -> Decision {
        m.key(vk, true, SHIFT);
        m.key(vk, false, SHIFT)
    }

    #[test]
    fn a_quick_tap_still_types_the_capital_once_no_second_tap_comes() {
        let mut m = Machine::default();
        let press = m.key(VK_V, true, SHIFT);
        assert!(press.swallow);
        assert_eq!(press.arm_timer, Some(HOLD_MS));
        let release = m.key(VK_V, false, SHIFT);
        assert!(release.swallow && release.replay.is_none(), "it might still become a double tap");
        assert_eq!(release.arm_timer, Some(DOUBLE_TAP_MS));
        assert_eq!(m.timer(), Expired::Replay(VK_V));
    }

    #[test]
    fn holding_starts_the_chord_and_releasing_stops_it() {
        let mut m = Machine::default();
        m.key(VK_V, true, SHIFT);
        assert_eq!(m.timer(), Expired::Signal(Signal::Start(Chord::Voice)));
        assert!(m.key(VK_V, true, SHIFT).swallow, "key repeat types nothing while recording");
        let release = m.key(VK_V, false, SHIFT);
        assert_eq!(release.signal, Some(Signal::Stop(Chord::Voice)));
        assert!(release.swallow && release.replay.is_none());
    }

    #[test]
    fn holding_c_takes_a_screenshot() {
        let mut m = Machine::default();
        m.key(VK_C, true, SHIFT);
        assert_eq!(m.timer(), Expired::Signal(Signal::Start(Chord::Shot)));
    }

    #[test]
    fn a_double_tap_of_c_takes_a_screenshot_and_types_nothing() {
        let mut m = Machine::default();
        tap(&mut m, VK_C);
        let second = m.key(VK_C, true, SHIFT);
        assert_eq!(second.signal, Some(Signal::Start(Chord::Shot)));
        assert!(second.swallow && second.replay.is_none() && second.disarm_timer);
        assert!(m.key(VK_C, false, SHIFT).swallow, "the second release types nothing");
        assert_eq!(m.timer(), Expired::Nothing);
    }

    #[test]
    fn a_double_tap_of_v_locks_dictation_on_until_pressed_again() {
        let mut m = Machine::default();
        tap(&mut m, VK_V);
        assert_eq!(m.key(VK_V, true, SHIFT).signal, Some(Signal::StartLocked(Chord::Voice)));
        assert!(m.key(VK_V, false, SHIFT).swallow);
        assert_eq!(m.key(VK_LSHIFT, false, NONE), Decision::default(), "letting go of Shift doesn't stop it");
        assert_eq!(m.timer(), Expired::Nothing);
        let stop = m.key(VK_V, true, SHIFT);
        assert_eq!(stop.signal, Some(Signal::Stop(Chord::Voice)));
        assert!(stop.swallow);
        assert!(m.key(VK_V, false, SHIFT).swallow);
        assert_eq!(m.key(VK_V, true, NONE), Decision::default(), "v types normally afterwards");
    }

    #[test]
    fn shift_b_talks_to_jarvis_held_or_locked_and_a_tap_still_types_a_capital() {
        let mut m = Machine::default();
        assert!(m.key(VK_B, true, SHIFT).swallow);
        assert_eq!(m.timer(), Expired::Signal(Signal::Start(Chord::Jarvis)));
        assert_eq!(m.key(VK_B, false, SHIFT).signal, Some(Signal::Stop(Chord::Jarvis)));
        let mut m = Machine::default();
        m.key(VK_B, true, SHIFT);
        m.key(VK_B, false, SHIFT);
        assert_eq!(m.key(VK_B, true, SHIFT).signal, Some(Signal::StartLocked(Chord::Jarvis)));
        m.key(VK_B, false, SHIFT);
        assert_eq!(m.key(VK_B, true, SHIFT).signal, Some(Signal::Stop(Chord::Jarvis)));
        let mut m = Machine::default();
        m.key(VK_B, true, SHIFT);
        m.key(VK_B, false, SHIFT);
        assert_eq!(m.timer(), Expired::Replay(VK_B));
    }

    #[test]
    fn you_can_still_type_and_screenshot_while_dictation_is_locked() {
        let mut m = Machine::default();
        tap(&mut m, VK_V);
        m.key(VK_V, true, SHIFT);
        m.key(VK_V, false, SHIFT);
        assert_eq!(m.key(VK_V, true, NONE), Decision::default(), "plain v");
        assert_eq!(m.key(VK_V, false, NONE), Decision::default());
        assert_eq!(m.key(VK_A, true, SHIFT), Decision::default(), "other capitals");
        assert_eq!(m.key(VK_A, false, SHIFT), Decision::default());
        m.key(VK_C, true, SHIFT);
        assert_eq!(m.timer(), Expired::Signal(Signal::Start(Chord::Shot)));
    }

    #[test]
    fn a_letter_you_just_typed_being_released_doesnt_cancel_the_hold() {
        let mut m = Machine::default();
        m.key(VK_C, true, SHIFT);
        assert_eq!(m.key(VK_A, false, SHIFT), Decision::default(), "the previous letter's release just passes through");
        assert_eq!(m.timer(), Expired::Signal(Signal::Start(Chord::Shot)));
    }

    #[test]
    fn fast_typing_keeps_its_letters_in_order() {
        let mut m = Machine::default();
        m.key(VK_V, true, SHIFT);
        let during_press = m.key(VK_A, true, NONE);
        assert_eq!(during_press.replay, Some(VK_V), "the V goes first");
        assert!(during_press.swallow && during_press.reinject && during_press.disarm_timer);
        assert_eq!(m.timer(), Expired::Nothing);

        let mut m = Machine::default();
        tap(&mut m, VK_V);
        let after_tap = m.key(VK_A, true, NONE);
        assert_eq!(after_tap.replay, Some(VK_V));
        assert!(after_tap.swallow && after_tap.reinject);
    }

    #[test]
    fn shift_release_during_the_double_tap_wait_passes_and_keeps_waiting() {
        let mut m = Machine::default();
        tap(&mut m, VK_V);
        assert_eq!(m.key(VK_LSHIFT, false, NONE), Decision::default(), "Shift itself must really be released");
        assert_eq!(m.timer(), Expired::Replay(VK_V));
    }

    #[test]
    fn capital_then_lowercase_of_the_same_letter_types_both() {
        let mut m = Machine::default();
        tap(&mut m, VK_V);
        let lower = m.key(VK_V, true, NONE);
        assert_eq!(lower.replay, Some(VK_V));
        assert!(lower.reinject);
    }

    #[test]
    fn letting_go_of_shift_first_still_types_the_capital() {
        let mut m = Machine::default();
        m.key(VK_V, true, SHIFT);
        let shift_up = m.key(VK_LSHIFT, false, NONE);
        assert_eq!(shift_up.replay, Some(VK_V));
        assert!(shift_up.reinject);
    }

    #[test]
    fn a_late_replay_presses_shift_itself_so_it_stays_a_capital() {
        // SAFETY: the union member is the one these inputs were built with.
        let keys = |inputs: Vec<INPUT>| -> Vec<u16> { inputs.iter().map(|input| unsafe { input.Anonymous.ki.wVk.0 }).collect() };
        assert_eq!(keys(capital_inputs(VK_V, true)), [VK_V, VK_V]);
        assert_eq!(keys(capital_inputs(VK_V, false)), [VK_SHIFT.0, VK_V, VK_V, VK_SHIFT.0]);
    }

    #[test]
    fn other_programs_keep_their_shortcuts() {
        let mut m = Machine::default();
        let ctrl = Mods { shift: true, other: true };
        assert_eq!(m.key(VK_V, true, ctrl), Decision::default(), "Ctrl+Shift+V");
        assert_eq!(m.key(VK_C, true, ctrl), Decision::default(), "Ctrl+Shift+C");
        assert_eq!(m.timer(), Expired::Nothing);
    }

    #[test]
    fn plain_letters_and_other_capitals_are_untouched() {
        let mut m = Machine::default();
        assert_eq!(m.key(VK_V, true, NONE), Decision::default());
        assert_eq!(m.key(VK_V, false, NONE), Decision::default());
        assert_eq!(m.key(VK_A, true, SHIFT), Decision::default());
    }

    #[test]
    fn releasing_shift_while_holding_stops_and_never_types_a_lowercase_v() {
        let mut m = Machine::default();
        m.key(VK_V, true, SHIFT);
        m.timer();
        let shift_up = m.key(VK_LSHIFT, false, NONE);
        assert_eq!(shift_up.signal, Some(Signal::Stop(Chord::Voice)));
        assert!(!shift_up.swallow, "Shift itself must really be released");
        assert!(m.key(VK_V, true, NONE).swallow, "the V still auto-repeating is hidden");
        assert!(m.key(VK_V, false, NONE).swallow);
        assert_eq!(m.key(VK_V, true, NONE), Decision::default(), "and then v works again");
    }

    #[test]
    fn typing_is_not_blocked_while_holding() {
        let mut m = Machine::default();
        m.key(VK_V, true, SHIFT);
        m.timer();
        assert_eq!(m.key(VK_A, true, SHIFT), Decision::default());
    }
}
