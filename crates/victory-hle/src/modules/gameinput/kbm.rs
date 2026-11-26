//! Keyboard and mouse as a gamepad (`VICTORY_KBM=1`). What each key does is
//! a text ([`DEFAULT`], or the file `VICTORY_KEYBINDS` names, read again
//! whenever it changes) with three parts: on foot, driving (keys not listed
//! there do what they do on foot) and menus. The title asks for no mouse, so
//! it has no pointer to move: in menus the wheel and the buttons stand for
//! the pad's, on foot the mouse turns the right stick.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

use super::*;

/// The mouse since the last poll: buttons held (virtual keys 1 to 6), pixels
/// moved (right and down positive) and wheel notches (up positive).
#[derive(Clone, Default)]
pub(super) struct Mouse {
    pub(super) buttons: Vec<u8>,
    pub(super) moved: (i32, i32),
    pub(super) wheel: i32,
}

/// Virtual keys Windows leaves undefined, for the wheel's two ways.
const WHEEL_UP: u8 = 0x0A;
const WHEEL_DOWN: u8 = 0x0B;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Button(u32),
    LeftTrigger,
    RightTrigger,
    Ahead,
    Back,
    Left,
    Right,
    Switch(Switch),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Switch {
    Drive,
    Menu,
    Mouse,
}

/// Which part of the bindings is in force.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Part {
    Foot,
    Drive,
    Menu,
}

/// The bindings without a file, and what a new file says.
pub(super) const DEFAULT: &str = "\
# Keyboard and mouse. One line per key: key = action. Saved changes take
# effect at once, also while the game runs. A # starts a comment.
#
# Keys:    A-Z 0-9 F1-F12 Up Down Left Right Enter Esc Tab Space Backspace
#          Shift Ctrl Alt CapsLock Mouse1-Mouse5 WheelUp WheelDown
#          (Mouse1 left, Mouse2 right, Mouse3 middle, Mouse4/5 side buttons)
# Actions: A B X Y LB RB LT RT LS RS (stick clicks) Menu View
#          DpadUp DpadDown DpadLeft DpadRight
#          Forward Back StrafeLeft StrafeRight (left stick)
#          ToggleDrive ToggleMenu ToggleMouse
# The mouse turns the right stick; VICTORY_MOUSE is its speed.

[foot]
W = Forward
S = Back
A = StrafeLeft
D = StrafeRight
Mouse1 = RT
Mouse2 = LT
Mouse3 = RS
Mouse4 = LB
Mouse5 = RB
WheelUp = DpadLeft
WheelDown = DpadRight
Shift = A
Space = X
R = B
F = Y
E = DpadRight
Q = RB
Tab = LB
Ctrl = LS
V = View
X = X
Y = Y
Enter = A
Backspace = B
Up = DpadUp
Down = DpadDown
Left = DpadLeft
Right = DpadRight
Esc = Menu
CapsLock = ToggleDrive
F10 = ToggleMenu
F9 = ToggleMouse

[drive]
W = RT
S = LT
Space = RB

[menu]
WheelUp = DpadUp
WheelDown = DpadDown
Mouse1 = A
Mouse2 = B
Mouse4 = LB
Mouse5 = RB
W = DpadUp
S = DpadDown
A = DpadLeft
D = DpadRight
Q = LB
E = RB
Z = LT
C = RT
X = X
Y = Y
Enter = A
Space = A
Backspace = B
Up = DpadUp
Down = DpadDown
Left = DpadLeft
Right = DpadRight
Tab = View
Esc = Menu
F10 = ToggleMenu
F9 = ToggleMouse
";

type Binds = HashMap<(Part, u8), Vec<Action>>;

fn key_named(name: &str) -> Option<u8> {
    let name = name.to_ascii_lowercase();
    let bytes = name.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphanumeric() {
        return Some(bytes[0].to_ascii_uppercase());
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
        && (1..=12).contains(&n)
    {
        return Some(0x6F + n);
    }
    Some(match &name[..] {
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "enter" => 0x0D,
        "esc" => 0x1B,
        "tab" => 0x09,
        "space" => 0x20,
        "backspace" => 0x08,
        "shift" => 0xA0,
        "ctrl" => 0xA2,
        "alt" => 0xA4,
        "capslock" => 0x14,
        "mouse1" => 0x01,
        "mouse2" => 0x02,
        "mouse3" => 0x04,
        "mouse4" => 0x05,
        "mouse5" => 0x06,
        "wheelup" => WHEEL_UP,
        "wheeldown" => WHEEL_DOWN,
        _ => return None,
    })
}

fn action_named(name: &str) -> Option<Action> {
    Some(match &name.to_ascii_lowercase()[..] {
        "a" => Action::Button(A),
        "b" => Action::Button(B),
        "x" => Action::Button(X),
        "y" => Action::Button(Y),
        "lb" => Action::Button(LEFT_SHOULDER),
        "rb" => Action::Button(RIGHT_SHOULDER),
        "ls" => Action::Button(LEFT_STICK),
        "rs" => Action::Button(RIGHT_STICK),
        "menu" => Action::Button(MENU),
        "view" => Action::Button(VIEW),
        "dpadup" => Action::Button(UP),
        "dpaddown" => Action::Button(DOWN),
        "dpadleft" => Action::Button(LEFT),
        "dpadright" => Action::Button(RIGHT),
        "lt" => Action::LeftTrigger,
        "rt" => Action::RightTrigger,
        "forward" => Action::Ahead,
        "back" => Action::Back,
        "strafeleft" => Action::Left,
        "straferight" => Action::Right,
        "toggledrive" => Action::Switch(Switch::Drive),
        "togglemenu" => Action::Switch(Switch::Menu),
        "togglemouse" => Action::Switch(Switch::Mouse),
        _ => return None,
    })
}

/// The bindings `text` says, and the lines that say nothing.
fn parse(text: &str) -> (Binds, Vec<String>) {
    let (mut binds, mut wrong) = (Binds::new(), Vec::new());
    let mut part = Some(Part::Foot);
    for line in text.trim_start_matches('\u{feff}').lines() {
        let said = line.split('#').next().unwrap_or_default().trim();
        if said.is_empty() {
            continue;
        }
        if said.starts_with('[') {
            part = match &said.to_ascii_lowercase()[..] {
                "[foot]" => Some(Part::Foot),
                "[drive]" => Some(Part::Drive),
                "[menu]" => Some(Part::Menu),
                _ => None,
            };
            if part.is_none() {
                wrong.push(said.to_owned());
            }
            continue;
        }
        let bind = said.split_once('=').and_then(|(key, action)| {
            Some((part?, key_named(key.trim())?, action_named(action.trim())?))
        });
        match bind {
            Some((part, key, action)) => binds.entry((part, key)).or_default().push(action),
            None => wrong.push(said.to_owned()),
        }
    }
    (binds, wrong)
}

/// Keyboard and mouse mode between polls.
pub(super) struct Kbm {
    binds: Binds,
    file: Option<PathBuf>,
    /// The file's time of change when it was read, and the next look at it.
    read: Option<SystemTime>,
    look_at: Option<Instant>,
    /// Right stick per mouse speed (`VICTORY_MOUSE`, 1 by default).
    sensitivity: f32,
    driving: bool,
    menu: bool,
    /// Whether the menu bindings were left by themselves once the world
    /// was shown ([`Kbm::auto_play`]).
    played: bool,
    released: bool,
    /// Keys down at the last poll: a switch acts when its key goes down.
    was: Vec<u8>,
    /// Movements of the last [`LOOK_OVER`]: the title reads the stick once a
    /// frame, which may be many polls, so one poll's movement is not it.
    moves: VecDeque<(Instant, i32, i32)>,
    /// Wheel notches not pressed yet, and the press under way.
    notches: i32,
    press: Option<(u8, Instant)>,
}

/// The right stick shows the mouse's speed over this long.
const LOOK_OVER: Duration = Duration::from_millis(48);
/// The mouse speed (pixels a millisecond) of a full right stick.
const LOOK_FULL: f32 = 1.6;
/// Where the stick starts: what titles pass over as resting.
const LOOK_START: f32 = 0.25;
/// A notch of the wheel is a press this long and a rest as long.
const NOTCH: Duration = Duration::from_millis(70);
/// Notches waiting at most: the menu must not run on after the wheel.
const NOTCHES: i32 = 8;
const LOOK_AT_FILE: Duration = Duration::from_secs(1);

impl Kbm {
    pub(super) fn new(sensitivity: f32) -> Kbm {
        Kbm {
            binds: parse(DEFAULT).0,
            file: None,
            read: None,
            look_at: None,
            sensitivity,
            driving: false,
            // A title starts in its menus.
            menu: true,
            played: false,
            released: false,
            was: Vec::new(),
            moves: VecDeque::new(),
            notches: 0,
            press: None,
        }
    }

    /// `VICTORY_KBM=1` (anything but 0): the mode, else none.
    pub(super) fn from_env() -> Option<Kbm> {
        let on = std::env::var_os("VICTORY_KBM").is_some_and(|v| !v.is_empty() && v != "0");
        on.then(|| {
            let sensitivity = std::env::var("VICTORY_MOUSE")
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(1.0);
            let mut kbm = Kbm::new(sensitivity);
            kbm.file = std::env::var_os("VICTORY_KEYBINDS")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from);
            tracing::info!(sensitivity, file = ?kbm.file, "keyboard and mouse instead of controllers");
            kbm
        })
    }

    /// Whether the pointer stays in the title's window.
    pub(super) fn holds_mouse(&self) -> bool {
        !self.released && !self.menu
    }

    /// Reads the bindings file again if it changed.
    fn reread(&mut self, now: Instant) {
        let Some(file) = &self.file else { return };
        if self.look_at.is_some_and(|at| now < at) {
            return;
        }
        self.look_at = Some(now + LOOK_AT_FILE);
        let Ok(changed) = std::fs::metadata(file).and_then(|m| m.modified()) else {
            return;
        };
        if self.read == Some(changed) {
            return;
        }
        // Not read (an editor may hold it): looked at again.
        let Ok(text) = std::fs::read_to_string(file) else {
            return;
        };
        self.read = Some(changed);
        let (binds, wrong) = parse(&text);
        if !wrong.is_empty() {
            tracing::warn!(?wrong, "key bindings: lines that say nothing");
        }
        // A file with nothing in it (being written) changes nothing.
        if !binds.is_empty() {
            tracing::info!(keys = binds.len(), "key bindings read");
            self.binds = binds;
        }
    }

    fn actions(&self, key: u8) -> &[Action] {
        let part = match (self.menu, self.driving) {
            (true, _) => Part::Menu,
            (false, true) => Part::Drive,
            (false, false) => Part::Foot,
        };
        self.binds
            .get(&(part, key))
            .or_else(|| {
                (part == Part::Drive)
                    .then(|| self.binds.get(&(Part::Foot, key)))
                    .flatten()
            })
            .map_or(&[], Vec::as_slice)
    }

    /// Once, when the title first shows its world: the play bindings, so
    /// that W A S D walk without F10 first (`VICTORY_KBM_AUTO_PLAY=0`: not).
    /// F10 and Esc switch as before.
    fn auto_play(&mut self) {
        if self.played || !self.menu || !victory_gpu::queue::world_shown() {
            return;
        }
        self.played = true;
        if std::env::var_os("VICTORY_KBM_AUTO_PLAY").is_some_and(|v| v == "0") {
            return;
        }
        self.menu = false;
        tracing::info!("keyboard and mouse: the world is shown, play bindings from now on (F10 or Esc: menu bindings)");
    }

    /// The gamepad these keys and this mouse make.
    pub(super) fn pad(&mut self, keys: &[KeyState], mouse: &Mouse, now: Instant) -> PadState {
        self.reread(now);
        self.auto_play();
        let mut held: Vec<u8> = keys.iter().map(|k| k.virtual_key).collect();
        held.extend(&mouse.buttons);
        held.extend(self.notch(mouse.wheel, now));

        // The switches first, by what the keys do before any of them acts.
        let fresh: Vec<u8> = held.iter().copied().filter(|k| !self.was.contains(k)).collect();
        let mut switched = Vec::new();
        for &key in &fresh {
            switched.extend(self.actions(key).iter().filter_map(|a| match a {
                Action::Switch(s) => Some(*s),
                // Opening and closing the title's menu changes the part too.
                Action::Button(b) if *b == MENU => Some(Switch::Menu),
                _ => None,
            }));
        }
        let mut out = PadState::default();
        for &key in &held {
            for &action in self.actions(key) {
                match action {
                    Action::Button(b) => out.buttons |= b,
                    Action::LeftTrigger => out.left_trigger = 1.0,
                    Action::RightTrigger => out.right_trigger = 1.0,
                    Action::Ahead => out.left_y += 1.0,
                    Action::Back => out.left_y -= 1.0,
                    Action::Left => out.left_x -= 1.0,
                    Action::Right => out.left_x += 1.0,
                    Action::Switch(_) => {}
                }
            }
        }
        out.left_x = out.left_x.clamp(-1.0, 1.0);
        out.left_y = out.left_y.clamp(-1.0, 1.0);
        for s in switched {
            match s {
                Switch::Drive => self.driving = !self.driving,
                Switch::Menu => self.menu = !self.menu,
                Switch::Mouse => self.released = !self.released,
            }
        }
        self.was = held;
        (out.right_x, out.right_y) = if self.holds_mouse() {
            self.look(mouse.moved, now)
        } else {
            self.moves.clear();
            (0.0, 0.0)
        };
        out
    }

    /// The wheel as a key: held for [`NOTCH`] per notch, with a rest between.
    fn notch(&mut self, wheel: i32, now: Instant) -> Option<u8> {
        self.notches = (self.notches + wheel).clamp(-NOTCHES, NOTCHES);
        if let Some((key, at)) = self.press {
            let age = now.duration_since(at);
            if age < NOTCH {
                return Some(key);
            }
            if age < NOTCH * 2 {
                return None;
            }
            self.press = None;
        }
        if self.notches == 0 {
            return None;
        }
        let key = if self.notches > 0 { WHEEL_UP } else { WHEEL_DOWN };
        self.notches -= self.notches.signum();
        self.press = Some((key, now));
        Some(key)
    }

    /// The right stick for what the mouse moved lately.
    fn look(&mut self, moved: (i32, i32), now: Instant) -> (f32, f32) {
        if moved != (0, 0) {
            self.moves.push_back((now, moved.0, moved.1));
        }
        while let Some(&(at, ..)) = self.moves.front()
            && now.duration_since(at) >= LOOK_OVER
        {
            self.moves.pop_front();
        }
        let (x, y) = self.moves.iter().fold((0.0, 0.0), |(x, y), &(_, dx, dy)| {
            (x + dx as f32, y + dy as f32)
        });
        let length = f32::hypot(x, y);
        if length == 0.0 {
            return (0.0, 0.0);
        }
        let speed = length / (LOOK_OVER.as_secs_f32() * 1000.0) / LOOK_FULL * self.sensitivity;
        let stick = LOOK_START + (1.0 - LOOK_START) * speed.min(1.0);
        // The mouse's down is the stick's negative.
        (x / length * stick, -y / length * stick)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(list: &[u8]) -> Vec<KeyState> {
        list.iter()
            .map(|&virtual_key| KeyState {
                virtual_key,
                ..Default::default()
            })
            .collect()
    }

    /// On foot, at rest.
    fn foot() -> (Kbm, Instant, Mouse) {
        let mut kbm = Kbm::new(1.0);
        kbm.menu = false;
        (kbm, Instant::now(), Mouse::default())
    }

    #[test]
    fn the_default_bindings_all_say_something() {
        let (binds, wrong) = parse(DEFAULT);
        assert_eq!(wrong, Vec::<String>::new());
        assert!(binds.len() > 60);
    }

    #[test]
    fn keys_on_foot_and_driving() {
        let (mut kbm, start, none) = foot();
        assert_eq!(kbm.pad(&[], &none, start), PadState::default());
        let walk = kbm.pad(&keys(&[b'W', b'A', 0xA0, 0x20]), &none, start);
        assert_eq!((walk.left_x, walk.left_y), (-1.0, 1.0));
        assert_eq!(walk.buttons, A | X);
        let mouse = Mouse {
            buttons: vec![0x01, 0x02, 0x04],
            ..Default::default()
        };
        let shot = kbm.pad(&[], &mouse, start);
        assert_eq!((shot.left_trigger, shot.right_trigger), (1.0, 1.0));
        assert_eq!(shot.buttons, RIGHT_STICK);
        kbm.pad(&[], &none, start);
        // Caps Lock: once per press, however long it is held.
        let caps = keys(&[0x14, b'W', b'D', 0x20]);
        kbm.pad(&caps, &none, start);
        for _ in 0..3 {
            let drive = kbm.pad(&caps, &none, start);
            assert_eq!((drive.right_trigger, drive.left_y, drive.left_x), (1.0, 0.0, 1.0));
            assert_eq!(drive.buttons, RIGHT_SHOULDER);
        }
        kbm.pad(&[], &none, start);
        kbm.pad(&keys(&[0x14]), &none, start);
        let off = kbm.pad(&keys(&[b'S']), &none, start);
        assert_eq!((off.left_y, off.left_trigger), (-1.0, 0.0));
    }

    #[test]
    fn menus_take_the_wheel_and_clicks() {
        let mut kbm = Kbm::new(1.0);
        let start = Instant::now();
        assert!(!kbm.holds_mouse());
        let click = Mouse {
            buttons: vec![0x01],
            moved: (50, 50),
            ..Default::default()
        };
        let pad = kbm.pad(&[], &click, start);
        assert_eq!((pad.buttons, pad.right_trigger, pad.right_x), (A, 0.0, 0.0));
        // Two notches down: two presses with a rest between them.
        let wheel = Mouse {
            wheel: -2,
            ..Default::default()
        };
        let none = Mouse::default();
        let mut seen = vec![kbm.pad(&[], &wheel, start).buttons];
        for step in 1..8 {
            seen.push(kbm.pad(&[], &none, start + NOTCH / 2 * step).buttons);
        }
        assert_eq!(seen, [DOWN, DOWN, 0, 0, DOWN, DOWN, 0, 0]);
        // Esc leaves the menu with the title's, and comes back to it.
        assert_eq!(kbm.pad(&keys(&[0x1B]), &none, start + NOTCH * 9).buttons, MENU);
        assert!(kbm.holds_mouse());
        kbm.pad(&[], &none, start + NOTCH * 10);
        kbm.pad(&keys(&[0x1B]), &none, start + NOTCH * 11);
        assert!(!kbm.holds_mouse());
    }

    #[test]
    fn the_mouse_can_be_let_go() {
        let (mut kbm, start, none) = foot();
        assert!(kbm.holds_mouse());
        kbm.pad(&keys(&[0x78]), &none, start);
        assert!(!kbm.holds_mouse());
        kbm.pad(&[], &none, start);
        kbm.pad(&keys(&[0x78]), &none, start);
        assert!(kbm.holds_mouse());
    }

    #[test]
    fn the_mouse_turns_the_right_stick_for_a_while() {
        let (mut kbm, start, _) = foot();
        let moved = |x, y| Mouse {
            moved: (x, y),
            ..Default::default()
        };
        let slow = kbm.pad(&[], &moved(1, 0), start);
        assert!(slow.right_x > LOOK_START && slow.right_x < 0.5);
        assert_eq!(slow.right_y.abs(), 0.0);
        // Up on the mouse is up on the stick, and the stick goes no further.
        let fast = kbm.pad(&[], &moved(0, -5000), start);
        assert!(fast.right_y > 0.99);
        assert!(f32::hypot(fast.right_x, fast.right_y) < 1.001);
        // Between movements it stays, then it is at rest.
        let soon = kbm.pad(&[], &moved(0, 0), start + Duration::from_millis(20));
        assert_eq!(soon.right_y, fast.right_y);
        let later = kbm.pad(&[], &moved(0, 0), start + LOOK_OVER);
        assert_eq!((later.right_x, later.right_y), (0.0, 0.0));
    }

    #[test]
    fn a_changed_file_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("keybinds.txt");
        std::fs::write(&file, "[foot]\nG = A # jump\nnonsense\nP = nothing\n").unwrap();
        let (mut kbm, start, none) = foot();
        kbm.file = Some(file.clone());
        assert_eq!(kbm.pad(&keys(&[b'G']), &none, start).buttons, A);
        assert_eq!(kbm.pad(&keys(&[b'P', b'W']), &none, start), PadState::default());
        std::fs::write(&file, "[foot]\nG = B\n").unwrap();
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|f| f.set_modified(later))
            .unwrap();
        assert_eq!(kbm.pad(&keys(&[b'G']), &none, start + LOOK_AT_FILE).buttons, B);
        // Emptied, or gone: the last bindings stay.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(kbm.pad(&keys(&[b'G']), &none, start + LOOK_AT_FILE * 2).buttons, B);
    }
}
