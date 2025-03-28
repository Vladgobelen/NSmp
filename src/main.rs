use clap::Parser;
use libc;
use rdev::{listen, Event as KbdEvent, EventType, Key, ListenError};
use rodio::{Decoder, OutputStream, Sink};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const SOCKET_PATH: &str = "/tmp/music_player.sock";
const PID_FILE: &str = "/tmp/music_player.pid";
const DEFAULT_CONFIG: &str = "music_player.json";

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(short, long)]
    path: Option<PathBuf>,

    #[arg(short = 'm', long)]
    cmd: Option<String>,

    #[arg(short, long)]
    config: Option<PathBuf>,

    #[arg(short, long, default_value_t = false)]
    daemon: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Config {
    hotkeys: HashMap<String, String>,
    music_dir: Option<String>,
    volume: f32,
}

impl Default for Config {
    fn default() -> Self {
        let mut hotkeys = HashMap::new();
        hotkeys.insert("next".to_string(), "XF86AudioNext".to_string());
        hotkeys.insert("prev".to_string(), "XF86AudioPrev".to_string());
        hotkeys.insert("pause".to_string(), "XF86AudioPlay".to_string());
        hotkeys.insert("stop".to_string(), "XF86AudioStop".to_string());

        Config {
            hotkeys,
            music_dir: None,
            volume: 0.7,
        }
    }
}

#[derive(Debug, Default)]
struct ModifierState {
    shift: bool,
    ctrl: bool,
    alt: bool,
    meta: bool,
}

impl ModifierState {
    fn update(&mut self, key: &Key, is_press: bool) {
        match key {
            Key::ShiftLeft | Key::ShiftRight => self.shift = is_press,
            Key::ControlLeft | Key::ControlRight => self.ctrl = is_press,
            Key::Alt | Key::AltGr => self.alt = is_press,
            Key::MetaLeft | Key::MetaRight => self.meta = is_press,
            _ => {}
        }
    }

    fn matches(&self, required_mods: &HashSet<&str>) -> bool {
        (required_mods.contains("shift") == self.shift)
            && (required_mods.contains("ctrl") == self.ctrl)
            && (required_mods.contains("alt") == self.alt)
            && (required_mods.contains("meta") == self.meta)
    }
}

fn main() -> Result<(), String> {
    let args = Args::parse();

    if let Some(cmd) = args.cmd {
        return send_command(&cmd);
    }

    let config_path = args.config.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG));
    let mut config = load_config(&config_path)?;

    if let Some(path) = args.path {
        config.music_dir = Some(path.to_string_lossy().into_owned());
        save_config(&config_path, &config)?;
    }

    let music_dir = match config.music_dir {
        Some(ref dir) => PathBuf::from(dir),
        None => PathBuf::from("."),
    };

    if args.daemon {
        daemonize()?;
    }

    let (_stream, handle) = OutputStream::try_default().map_err(|e| e.to_string())?;
    let sink = Arc::new(Mutex::new(
        Sink::try_new(&handle).map_err(|e| e.to_string())?,
    ));
    sink.lock().unwrap().set_volume(config.volume);

    let mut player = MusicPlayer::new(music_dir).map_err(|e| e.to_string())?;

    let _ = fs::remove_file(SOCKET_PATH);
    save_pid()?;

    let player_clone = player.clone();
    let sink_clone = Arc::clone(&sink);
    thread::spawn(move || {
        command_server(player_clone, sink_clone);
    });

    let config_clone = config.clone();
    thread::spawn(move || {
        if let Err(e) = hotkey_listener(config_clone) {
            eprintln!("Hotkey listener error: {:?}", e);
        }
    });

    player.main_loop(Arc::clone(&sink));
    Ok(())
}

fn daemonize() -> Result<(), String> {
    unsafe {
        match libc::fork() {
            -1 => Err("Failed to fork".to_string()),
            0 => Ok(()),
            _ => process::exit(0),
        }
    }
}

fn save_pid() -> Result<(), String> {
    fs::write(PID_FILE, process::id().to_string()).map_err(|e| e.to_string())
}

fn send_command(cmd: &str) -> Result<(), String> {
    let mut stream = UnixStream::connect(SOCKET_PATH).map_err(|e| e.to_string())?;
    stream
        .write_all(cmd.as_bytes())
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn load_config(path: &Path) -> Result<Config, String> {
    if path.exists() {
        let data = fs::read_to_string(path).map_err(|e| e.to_string())?;
        serde_json::from_str(&data).map_err(|e| e.to_string())
    } else {
        let config = Config::default();
        save_config(path, &config)?;
        Ok(config)
    }
}

fn save_config(path: &Path, config: &Config) -> Result<(), String> {
    let data = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    fs::write(path, data).map_err(|e| e.to_string())
}

fn hotkey_listener(config: Config) -> Result<(), ListenError> {
    let mut pressed_keys = HashSet::new();
    let mut modifiers = ModifierState::default();

    let callback = move |event: KbdEvent| match event.event_type {
        EventType::KeyPress(key) => {
            pressed_keys.insert(key.clone());
            modifiers.update(&key, true);

            for (cmd, key_combination) in &config.hotkeys {
                if check_hotkey(&pressed_keys, &modifiers, key_combination) {
                    let _ = send_command(cmd);
                }
            }
        }
        EventType::KeyRelease(key) => {
            pressed_keys.remove(&key);
            modifiers.update(&key, false);
        }
        _ => {}
    };

    listen(callback)
}

fn check_hotkey(pressed_keys: &HashSet<Key>, modifiers: &ModifierState, hotkey_str: &str) -> bool {
    let parts: Vec<&str> = hotkey_str.split('+').collect();
    let mut required_mods = HashSet::new();
    let mut required_key = None;

    for part in parts {
        match part.to_lowercase().as_str() {
            "shift" => required_mods.insert("shift"),
            "ctrl" => required_mods.insert("ctrl"),
            "alt" => required_mods.insert("alt"),
            "meta" | "super" | "win" => required_mods.insert("meta"),
            key_str => {
                required_key = str_to_key(key_str);
                false
            }
        };
    }

    modifiers.matches(&required_mods) && required_key.map_or(false, |k| pressed_keys.contains(&k))
}

fn str_to_key(key_str: &str) -> Option<Key> {
    match key_str.to_lowercase().as_str() {
        // Медиа-клавиши
        "nextsong" | "audionext" => Some(Key::Unknown(0x1008ff17)),
        "previoussong" | "audioprev" => Some(Key::Unknown(0x1008ff16)),
        "playpause" | "audioplay" => Some(Key::Unknown(0x1008ff14)),
        "stopcd" | "audiostop" => Some(Key::Unknown(0x1008ff15)),
        "volumedown" => Some(Key::Unknown(0x1008ff11)),
        "volumeup" => Some(Key::Unknown(0x1008ff13)),
        "volumemute" => Some(Key::Unknown(0x1008ff12)),

        // Буквы
        "a" => Some(Key::KeyA),
        "b" => Some(Key::KeyB),
        "c" => Some(Key::KeyC),
        "d" => Some(Key::KeyD),
        "e" => Some(Key::KeyE),
        "f" => Some(Key::KeyF),
        "g" => Some(Key::KeyG),
        "h" => Some(Key::KeyH),
        "i" => Some(Key::KeyI),
        "j" => Some(Key::KeyJ),
        "k" => Some(Key::KeyK),
        "l" => Some(Key::KeyL),
        "m" => Some(Key::KeyM),
        "n" => Some(Key::KeyN),
        "o" => Some(Key::KeyO),
        "p" => Some(Key::KeyP),
        "q" => Some(Key::KeyQ),
        "r" => Some(Key::KeyR),
        "s" => Some(Key::KeyS),
        "t" => Some(Key::KeyT),
        "u" => Some(Key::KeyU),
        "v" => Some(Key::KeyV),
        "w" => Some(Key::KeyW),
        "x" => Some(Key::KeyX),
        "y" => Some(Key::KeyY),
        "z" => Some(Key::KeyZ),

        // Цифры
        "0" => Some(Key::Num0),
        "1" => Some(Key::Num1),
        "2" => Some(Key::Num2),
        "3" => Some(Key::Num3),
        "4" => Some(Key::Num4),
        "5" => Some(Key::Num5),
        "6" => Some(Key::Num6),
        "7" => Some(Key::Num7),
        "8" => Some(Key::Num8),
        "9" => Some(Key::Num9),

        // Функциональные клавиши
        "f1" => Some(Key::F1),
        "f2" => Some(Key::F2),
        "f3" => Some(Key::F3),
        "f4" => Some(Key::F4),
        "f5" => Some(Key::F5),
        "f6" => Some(Key::F6),
        "f7" => Some(Key::F7),
        "f8" => Some(Key::F8),
        "f9" => Some(Key::F9),
        "f10" => Some(Key::F10),
        "f11" => Some(Key::F11),
        "f12" => Some(Key::F12),

        // Специальные клавиши
        "space" => Some(Key::Space),
        "enter" => Some(Key::Return),
        "tab" => Some(Key::Tab),
        "backspace" => Some(Key::Backspace),
        "escape" => Some(Key::Escape),
        "insert" => Some(Key::Insert),
        "delete" => Some(Key::Delete),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "up" => Some(Key::UpArrow),
        "down" => Some(Key::DownArrow),
        "left" => Some(Key::LeftArrow),
        "right" => Some(Key::RightArrow),

        // Модификаторы
        "shift" => Some(Key::ShiftLeft),
        "ctrl" => Some(Key::ControlLeft),
        "alt" => Some(Key::Alt),
        "meta" | "super" | "win" => Some(Key::MetaLeft),

        _ => None,
    }
}

fn command_server(mut player: MusicPlayer, sink: Arc<Mutex<Sink>>) {
    let listener = UnixListener::bind(SOCKET_PATH).unwrap();

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let mut cmd = String::new();
                if stream.read_to_string(&mut cmd).is_ok() {
                    match cmd.as_str() {
                        "next" => {
                            let sink = sink.lock().unwrap();
                            let _ = player.next(&sink);
                        }
                        "prev" => {
                            let sink = sink.lock().unwrap();
                            let _ = player.prev(&sink);
                        }
                        "pause" => {
                            let sink = sink.lock().unwrap();
                            if sink.is_paused() {
                                sink.play();
                            } else {
                                sink.pause();
                            }
                        }
                        "stop" => process::exit(0),
                        "volume_up" => {
                            let sink = sink.lock().unwrap();
                            let vol = (sink.volume() + 0.1).min(1.0);
                            sink.set_volume(vol);
                        }
                        "volume_down" => {
                            let sink = sink.lock().unwrap();
                            let vol = (sink.volume() - 0.1).max(0.0);
                            sink.set_volume(vol);
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => eprintln!("Connection error: {}", e),
        }
    }
}

#[derive(Clone)]
struct MusicPlayer {
    files: Vec<PathBuf>,
    current_index: usize,
}

impl MusicPlayer {
    fn new(path: PathBuf) -> Result<Self, io::Error> {
        if !path.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Path is not a directory",
            ));
        }

        let supported = ["mp3", "wav", "flac", "ogg", "aac", "m4a"];
        let mut files = Vec::new();

        for entry in fs::read_dir(path)? {
            let path = entry?.path();
            if path.is_file() && has_supported_extension(&path, &supported) {
                files.push(path);
            }
        }

        if files.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "No supported audio files found",
            ));
        }

        Ok(Self {
            files,
            current_index: 0,
        })
    }

    fn main_loop(&mut self, sink: Arc<Mutex<Sink>>) {
        {
            let sink = sink.lock().unwrap();
            self.play(&sink).unwrap();
        }

        loop {
            {
                let sink = sink.lock().unwrap();
                if sink.empty() {
                    self.next(&sink).unwrap();
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn play(&self, sink: &Sink) -> Result<(), io::Error> {
        sink.stop();
        let file = fs::File::open(&self.files[self.current_index])?;
        let source = Decoder::new(file).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        sink.append(source);
        println!("Now playing: {}", self.current_track());
        Ok(())
    }

    fn next(&mut self, sink: &Sink) -> Result<(), io::Error> {
        self.current_index = (self.current_index + 1) % self.files.len();
        self.play(sink)
    }

    fn prev(&mut self, sink: &Sink) -> Result<(), io::Error> {
        self.current_index = if self.current_index == 0 {
            self.files.len() - 1
        } else {
            self.current_index - 1
        };
        self.play(sink)
    }

    fn current_track(&self) -> String {
        self.files[self.current_index]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
}

fn has_supported_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| extensions.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}
