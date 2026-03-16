use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::ptr;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::io::Write;
use std::os::unix::io::{FromRawFd, AsFd};
use std::collections::HashMap;
use std::process::Command;
use gtk::prelude::*;
use gtk::{Application, ApplicationWindow, Button, CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION, glib};
use gtk4_layer_shell::{Layer, Edge, KeyboardMode, LayerShell};
use glib::{Bytes, ControlFlow};
use serde::Deserialize;
use toml;
use std::fs::{self, read_to_string};
use std::env;

// D-Bus via zbus (replaces gdbus subprocess)
use zbus::blocking::Connection as ZbusConnection;

// nlprule grammar correction (English + German only)
use nlprule::{Rules, Tokenizer, rules_filename, tokenizer_filename};

// Wayland virtual keyboard (replaces wtype subprocess)
use wayland_client::{
    Connection as WlConnection, Dispatch, QueueHandle,
    protocol::{wl_registry, wl_seat},
    globals::{registry_queue_init, GlobalListContents},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

// ───────────────────────────────────────────────
// VOSK FFI
// ───────────────────────────────────────────────
#[link(name = "vosk")]
extern "C" {
    fn vosk_model_new(model_path: *const i8) -> *mut std::ffi::c_void;
    fn vosk_recognizer_new(model: *mut std::ffi::c_void, sample_rate: f32) -> *mut std::ffi::c_void;
    fn vosk_recognizer_accept_waveform(recognizer: *mut std::ffi::c_void, data: *const i8, len: u32) -> i32;
    fn vosk_recognizer_result(recognizer: *mut std::ffi::c_void) -> *const i8;
    fn vosk_recognizer_partial_result(recognizer: *mut std::ffi::c_void) -> *const i8;
    fn vosk_recognizer_reset(recognizer: *mut std::ffi::c_void);
    fn vosk_model_free(model: *mut std::ffi::c_void);
    fn vosk_recognizer_free(recognizer: *mut std::ffi::c_void);
}

// ───────────────────────────────────────────────
// PULSEAUDIO FFI
// ───────────────────────────────────────────────
#[repr(C)]
pub struct pa_sample_spec {
    format: i32,
    rate: u32,
    channels: u8,
}

#[link(name = "pulse")]
extern "C" {
    fn pa_context_new(api: *mut std::ffi::c_void, name: *const i8) -> *mut std::ffi::c_void;
    fn pa_context_connect(ctx: *mut std::ffi::c_void, addr: *const i8, flags: u32, api: *mut std::ffi::c_void) -> i32;
    fn pa_stream_new(
        ctx: *mut std::ffi::c_void,
        name: *const i8,
        spec: *const pa_sample_spec,
        map: *const std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn pa_stream_connect_record(
        s: *mut std::ffi::c_void,
        dev: *const i8,
        attr: *const std::ffi::c_void,
        flags: u32,
    ) -> i32;
    fn pa_stream_set_read_callback(
        s: *mut std::ffi::c_void,
        cb: Option<extern "C" fn(*mut std::ffi::c_void, usize, *mut std::ffi::c_void)>,
        userdata: *mut std::ffi::c_void,
    );
    fn pa_stream_peek(s: *mut std::ffi::c_void, data: *mut *const std::ffi::c_void, len: *mut usize) -> i32;
    fn pa_stream_drop(s: *mut std::ffi::c_void);
    fn pa_stream_disconnect(s: *mut std::ffi::c_void) -> i32;
    fn pa_stream_flush(s: *mut std::ffi::c_void) -> i32;
    fn pa_stream_unref(s: *mut std::ffi::c_void);
    fn pa_strerror(error: i32) -> *const i8;
}

#[link(name = "pulse-mainloop-glib")]
extern "C" {
    fn pa_glib_mainloop_new(gctx: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn pa_glib_mainloop_get_api(m: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

const PA_SAMPLE_S16LE: i32 = 3;
const SAMPLE_RATE: u32 = 16000;

// ───────────────────────────────────────────────
// SHARED STATE
// ───────────────────────────────────────────────
struct AudioState {
    recognizer: *mut std::ffi::c_void,
    model: *mut std::ffi::c_void,
    pa_glib_mainloop: *mut std::ffi::c_void,
    pa_context: *mut std::ffi::c_void,
    pa_stream: *mut std::ffi::c_void,
    is_recording: bool,
    is_model_loaded: bool,
    // Audio enhancement — None when disabled or unavailable.
    echo_cancel_module_id: Option<u32>,
    enhanced_source: Option<CString>,
}

thread_local! {
    static AUDIO_STATE: RefCell<Option<AudioState>> = RefCell::new(None);
    // Cached zbus session-bus connection; created once, reused for every OSK poll.
    static ZBUS_CONN: RefCell<Option<ZbusConnection>> = RefCell::new(None);
    // Language detected from model_size — drives punctuation and grammar logic.
    static VOICE_LANG: RefCell<VoiceLang> = RefCell::new(VoiceLang::English);
    // nlprule grammar engine — only populated for English and German.
    // None for all other languages; no RAM cost for non-supported languages.
    static NLPRULE: RefCell<Option<(Rules, Tokenizer)>> = RefCell::new(None);
    // Runtime feature toggles — written once from config in main(), read in
    // audio_callback (a C function pointer that cannot hold Rc/config refs).
    static FEATURE_FLAGS: RefCell<FeatureFlags> = RefCell::new(FeatureFlags::default());
}

// ───────────────────────────────────────────────
// FEATURE FLAGS
// Written once at startup from config; read cheaply
// from the audio callback via thread_local.
// ───────────────────────────────────────────────
#[derive(Clone, Copy)]
struct FeatureFlags {
    /// Capitalise first letter of each utterance (and, via nlprule, proper
    /// nouns/German nouns).  Skipped for CJK languages automatically.
    auto_capitalize: bool,
    /// Append a terminal '.' or '?' based on question-word heuristics.
    auto_punctuate: bool,
    /// Run nlprule grammar correction (English + German only).
    nlp_correction: bool,
}

impl Default for FeatureFlags {
    fn default() -> Self {
        Self { auto_capitalize: true, auto_punctuate: true, nlp_correction: true }
    }
}

// ───────────────────────────────────────────────
// LANGUAGE DETECTION
// Derived once from config.model_size at startup.
// ───────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VoiceLang {
    English,
    German,
    Russian,
    Dutch,
    French,
    Spanish,
    Italian,
    Swedish,
    Polish,
    // CJK languages use their own punctuation systems; we leave their
    // output untouched and do not run Harper on them.
    Korean,
    Chinese,
    Japanese,
}

impl VoiceLang {
    fn from_model_size(s: &str) -> Self {
        match s {
            "small-german"   => Self::German,
            "small-russian"  => Self::Russian,
            "small-dutch"    => Self::Dutch,
            "small-french"   => Self::French,
            "small-spanish"  => Self::Spanish,
            "small-italian"  => Self::Italian,
            "small-swedish"  => Self::Swedish,
            "small-polish"   => Self::Polish,
            "small-korean"   => Self::Korean,
            "small-chinese"  => Self::Chinese,
            "small-japanese" => Self::Japanese,
            _                => Self::English, // small / medium / large / unknown
        }
    }

    /// Only English and German are supported by nlprule.
    /// All other languages skip grammar correction entirely.
    fn uses_nlprule(self) -> bool {
        matches!(self, Self::English | Self::German)
    }

    /// CJK scripts do not use Latin-alphabet capitalisation.
    fn supports_capitalization(self) -> bool {
        !matches!(self, Self::Korean | Self::Chinese | Self::Japanese)
    }

    /// CJK scripts have their own punctuation conventions; don't inject
    /// Western sentence-ending marks into them.
    fn uses_western_punctuation(self) -> bool {
        !matches!(self, Self::Korean | Self::Chinese | Self::Japanese)
    }

    /// Question-starter words for this language.  These are the words that,
    /// when they begin an utterance, strongly suggest a question rather than
    /// a statement.
    fn question_starters(self) -> &'static [&'static str] {
        match self {
            Self::English => &[
                // Core WH-question words (interrogatives)
            "who", "what", "where", "when", "why", "how", "which", "whose", "whom",

            // WH- compounds / extensions (very frequent in real speech)
            "how many", "how much", "how long", "how far", "how often", "how old",
            "how come", "what kind", "what time", "what about", "what if", "why don't",
            "whoever", "whatever", "whenever", "wherever", "whichever",

            // Forms of BE (very common question openers)
            "is", "are", "was", "were", "am", "isn't", "aren't", "wasn't", "weren't",

            // Auxiliary DO forms
            "do", "does", "did", "don't", "doesn't", "didn't",

            // HAVE forms
            "have", "has", "had", "haven't", "hasn't", "hadn't",

            // Modals (extremely common)
            "can", "could", "will", "would", "shall", "should", "may", "might", "must",
            "can't", "couldn't", "won't", "wouldn't", "shan't", "shouldn't", "may not", "might not", "mustn't",

            // Contractions that frequently start casual questions
            "isn't", "aren't", "wasn't", "weren't",
            "don't", "doesn't", "didn't",
            "haven't", "hasn't", "hadn't",
            "can't", "couldn't", "won't", "wouldn't", "shouldn't", "mustn't",
            "who's", "what's", "where's", "when's", "why's", "how's",
            "who're", "what're", "where're", "when're",  // less common but do occur

            // Other frequent casual / spoken starters
            "are", "isn't it", "aren't you", "don't you", "didn't you",
            "can you", "could you", "will you", "would you",
            "shall we", "should I", "may I", "might I",
            ],
            Self::German => &[
                // W-Fragen
                "wer", "was", "wo", "wohin", "woher", "wann", "warum", "wie",
                "welcher", "welche", "welches",
                // Entscheidungsfragen start with a verb — list common auxiliaries
                "ist", "sind", "war", "waren", "bin",
                "hat", "haben", "hatte", "hatten",
                "wird", "werden", "würde", "würden",
                "kann", "können", "konnte",
                "darf", "dürfen", "muss", "müssen",
                "soll", "sollen",
            ],
            Self::Russian => &[
                // Вопросительные слова
                "кто", "что", "где", "куда", "откуда", "когда", "почему",
                "зачем", "как", "какой", "какая", "какое", "какие", "который",
                // Частицы и связки
                "ли", "разве", "неужели",
                "есть", "был", "была", "было", "были",
                "будет", "будут",
            ],
            Self::Dutch => &[
                "wie", "wat", "waar", "waarheen", "wanneer", "waarom", "hoe", "welke",
                "is", "zijn", "was", "waren",
                "heeft", "hebben", "had", "hadden",
                "kan", "kunnen", "mag", "zal", "zullen", "moet", "moeten",
            ],
            Self::French => &[
                "qui", "que", "quoi", "où", "quand", "pourquoi", "comment",
                "quel", "quelle", "quels", "quelles", "lequel",
                "est", "êtes", "es",
                "as", "avez", "avons", "ont",
                "peux", "pouvez", "peut", "peuvent",
                "faut", "dois", "devez",
            ],
            Self::Spanish => &[
                "quién", "qué", "dónde", "adónde", "cuándo", "por qué",
                "cómo", "cuál", "cuánto", "cuánta", "cuántos", "cuántas",
                "es", "son", "era", "eran",
                "has", "ha", "han", "había",
                "puede", "pueden", "puedes",
                "hay", "tengo", "tienes",
            ],
            Self::Italian => &[
                "chi", "che", "cosa", "dove", "quando", "perché", "come",
                "quale", "quali", "quanto", "quanta",
                "è", "sei", "siete", "sono",
                "hai", "ha", "abbiamo", "avete", "hanno",
                "puoi", "può", "possiamo",
                "devi", "deve",
            ],
            Self::Swedish => &[
                "vem", "vad", "var", "vart", "varifrån", "när", "varför", "hur",
                "vilken", "vilket", "vilka",
                "är", "var", "var",
                "har", "hade",
                "kan", "ska", "skall", "bör", "måste", "får",
            ],
            Self::Polish => &[
                "kto", "co", "gdzie", "dokąd", "skąd", "kiedy", "dlaczego",
                "jak", "który", "która", "które", "ile",
                "jest", "są", "był", "była", "było", "byli",
                "ma", "mają", "miał",
                "może", "mogą", "czy",
            ],
            // CJK — question/statement distinction is grammatically marked
            // differently (particles, intonation).  Return an empty list and
            // let add_terminal_punctuation use the language-specific path.
            Self::Korean | Self::Chinese | Self::Japanese => &[],
        }
    }
}

// ───────────────────────────────────────────────
// AUDIO CALLBACK
// ───────────────────────────────────────────────
extern "C" fn audio_callback(_stream: *mut std::ffi::c_void, _nbytes: usize, userdata: *mut std::ffi::c_void) {
    unsafe {
        if userdata.is_null() {
            eprintln!("Callback: null userdata");
            return;
        }
        let recognizer = userdata as *mut std::ffi::c_void;
        if recognizer.is_null() {
            eprintln!("Callback: null recognizer");
            return;
        }
        let mut data_ptr: *const std::ffi::c_void = ptr::null();
        let mut length: usize = 0;
        if pa_stream_peek(_stream, &mut data_ptr, &mut length) < 0 {
            eprintln!("pa_stream_peek failed");
            return;
        }
        if length > 0 && !data_ptr.is_null() {
            let res = vosk_recognizer_accept_waveform(recognizer, data_ptr as *const i8, length as u32);
            if res != 0 {
                let result_ptr = vosk_recognizer_result(recognizer);
                if !result_ptr.is_null() {
                    let json = CStr::from_ptr(result_ptr as *const u8).to_string_lossy();
                    let text = clean_vosk_text(&json);
                    if !text.trim().is_empty() {
                        let flags = FEATURE_FLAGS.with(|f| *f.borrow());
                        let lang  = VOICE_LANG.with(|c| *c.borrow());

                        // 1. Terminal punctuation
                        let out = if flags.auto_punctuate {
                            add_terminal_punctuation(&text)
                        } else {
                            text.clone()
                        };

                        // 2. Sentence-start capitalisation
                        //    Skipped for CJK; nlprule (step 3) handles proper
                        //    nouns / German noun caps when also enabled.
                        let out = if flags.auto_capitalize && lang.supports_capitalization() {
                            capitalize_first(&out)
                        } else {
                            out
                        };

                        // 3. NLP grammar correction (en/de only)
                        let out = if flags.nlp_correction {
                            apply_grammar_corrections(&out)
                        } else {
                            out
                        };

                        println!("{} → {}", text, out);
                        insert_text_into_focused_field(&out);
                    }
                }
            } else {
                let partial_ptr = vosk_recognizer_partial_result(recognizer);
                if !partial_ptr.is_null() {
                    let json = CStr::from_ptr(partial_ptr as *const u8).to_string_lossy();
                    let partial = clean_vosk_text(&json);
                    if !partial.trim().is_empty() {
                        println!("Partial: {}", partial);
                    }
                }
            }
        }
        pa_stream_drop(_stream);
    }
}

fn clean_vosk_text(json: &str) -> String {
    let mut s = json.trim().to_string();
    let start_patterns = ["\"text\" : \"", "\"text\":\"", "\"partial\" : \"", "\"partial\":\""];
    let mut value_start = 0;
    for pattern in start_patterns.iter() {
        if let Some(idx) = s.find(pattern) {
            value_start = idx + pattern.len();
            break;
        }
    }
    if value_start == 0 {
        return s.trim_matches(&['{', '}', '"', ':', ' '][..]).to_string();
    }
    s = s[value_start..].to_string();
    if let Some(end) = s.find('"') {
        s = s[..end].to_string();
    }
    s.trim_matches(&['"', ':', ' '][..]).trim().to_string()
}

// ───────────────────────────────────────────────
// PUNCTUATION RESTORATION
// Vosk outputs raw words with no punctuation.
// Language-aware: uses per-language question starters,
// skips Western punctuation for CJK scripts entirely.
//
// Pipeline:  vosk → add_terminal_punctuation → apply_grammar_corrections → type
// ───────────────────────────────────────────────

fn add_terminal_punctuation(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }

    let lang = VOICE_LANG.with(|c| *c.borrow());

    // CJK: leave the text completely untouched — the models sometimes
    // already include native punctuation, and injecting Western marks
    // would produce malformed output.
    if !lang.uses_western_punctuation() {
        return trimmed.to_string();
    }

    // Don't double-add punctuation if Vosk already supplied it.
    if matches!(trimmed.chars().last(), Some('.' | '?' | '!' | '…')) {
        return trimmed.to_string();
    }

    let first_word = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    let first_word = first_word.trim_matches(|c: char| !c.is_alphabetic());

    if lang.question_starters().contains(&first_word) {
        format!("{}?", trimmed)
    } else {
        format!("{}.", trimmed)
    }
}

// ───────────────────────────────────────────────
// TEXT HELPERS
// ───────────────────────────────────────────────

/// Capitalise only the very first character of a string.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None    => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

// ───────────────────────────────────────────────
// NLPRULE GRAMMAR CORRECTION
// Applies rule-based grammatical error correction
// using nlprule (LanguageTool rules, Rust-native).
// Only active for English and German — all other
// languages return the input unchanged, and the
// NLPRULE thread_local stays None (zero RAM cost).
//
// Capitalisation is applied upstream in the pipeline
// (step 2 of the audio callback) before this is called,
// so nlprule's POS tagger always sees a proper sentence.
// ───────────────────────────────────────────────
fn apply_grammar_corrections(text: &str) -> String {
    NLPRULE.with(|cell| {
        let opt = cell.borrow();
        match opt.as_ref() {
            Some((rules, tokenizer)) => {
                let corrected = rules.correct(text, tokenizer);
                if corrected != text {
                    println!("nlprule: {:?} → {:?}", text, corrected);
                }
                corrected
            }
            // Language not supported by nlprule — pass through unchanged.
            None => text.to_string(),
        }
    })
}

// ───────────────────────────────────────────────
// WAYLAND VIRTUAL KEYBOARD
// Replaces the wtype subprocess with direct use of
// zwp_virtual_keyboard_unstable_v1. A fresh Wayland
// connection is opened per utterance (acceptable since
// insert_text is only called at sentence boundaries).
// ───────────────────────────────────────────────

/// Minimal app-data type required by wayland-client's Dispatch machinery.
struct VkAppData;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for VkAppData {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        // GlobalListContents is maintained internally by registry_queue_init.
    }
}

// Neither manager nor virtual-keyboard send any events back to the client,
// so a no-op Dispatch implementation is sufficient for both.
wayland_client::delegate_noop!(VkAppData: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(VkAppData: ignore ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(VkAppData: ignore ZwpVirtualKeyboardV1);

/// Build a minimal XKB keymap that assigns one dedicated keycode to every
/// unique character that needs to be typed. Keycodes start at XKB 9
/// (= Linux evdev 1) so they do not collide with real hardware keys when
/// processed through this isolated virtual keyboard.
fn generate_keymap(chars: &[char]) -> String {
    // Start at XKB 16 (evdev 8).  evdev 0-7 covers kernel-reserved codes
    // (ESC, digit row, etc.) that some compositors treat specially even with
    // a custom keymap loaded.  Starting at 16 keeps us well clear of them.
    const XKB_BASE: usize = 16; // evdev = XKB - 8 = 8
    let max_xkb = (XKB_BASE + chars.len().saturating_sub(1)).max(XKB_BASE) as u32;
    let mut keycodes = String::new();
    let mut symbols = String::new();

    for (i, &c) in chars.iter().enumerate() {
        let xkb_kc = XKB_BASE + i; // evdev = xkb_kc - 8 = i + 8
        let name = format!("VK{:03}", i);
        keycodes.push_str(&format!("        <{}> = {};\n", name, xkb_kc));
        symbols.push_str(&format!("        key <{}> {{ [ U{:04X} ] }};\n", name, c as u32));
    }

    format!(
        "xkb_keymap {{\n\
         \txkb_keycodes \"vk\" {{\n\
         \t\tminimum = 8;\n\
         \t\tmaximum = {};\n\
         {}\t}};\n\
         \txkb_types \"vk\" {{ include \"complete\" }};\n\
         \txkb_compat \"vk\" {{ include \"complete\" }};\n\
         \txkb_symbols \"vk\" {{\n\
         {}\t}};\n\
         \txkb_geometry \"vk\" {{}};\n\
         }};",
        max_xkb, keycodes, symbols
    )
}

/// Monotonic-ish timestamp in milliseconds for Wayland key events.
fn now_ms() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

/// Type `text` into whatever surface currently has keyboard focus by
/// injecting key events through the Wayland virtual keyboard protocol.
fn insert_text_into_focused_field(text: &str) {
    if text.is_empty() {
        return;
    }

    // Prepend a space so dictated words don't run into existing text.
    let text_spaced = format!(" {}", text);

    // Collect unique characters (first-occurrence order).
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<char> = text_spaced.chars().filter(|c| seen.insert(*c)).collect();

    // Map each unique char to an evdev keycode.
    //   XKB keycode = index + 16  →  evdev keycode = XKB - 8 = index + 8
    let char_to_kc: HashMap<char, u32> = unique
        .iter()
        .enumerate()
        .map(|(i, &c)| (c, (i + 8) as u32))
        .collect();

    let keymap_str = generate_keymap(&unique);

    // Open a dedicated Wayland connection.  GTK's own connection is not
    // accessible from Rust, and having two connections to the compositor is
    // perfectly valid.
    let conn = match WlConnection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("VK: Wayland connect failed: {}", e);
            return;
        }
    };

    let (globals, mut queue) = match registry_queue_init::<VkAppData>(&conn) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("VK: registry init failed: {}", e);
            return;
        }
    };

    let qh = queue.handle();
    let mut state = VkAppData;

    let seat = match globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=7, ()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("VK: compositor has no wl_seat: {}", e);
            return;
        }
    };

    let vk_manager = match globals.bind::<ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("VK: compositor has no zwp_virtual_keyboard_manager_v1: {}", e);
            return;
        }
    };

    // Flush the bind requests and process any initial events.
    if let Err(e) = queue.roundtrip(&mut state) {
        eprintln!("VK: initial roundtrip failed: {}", e);
        return;
    }

    let vk = vk_manager.create_virtual_keyboard(&seat, &qh, ());

    // ── Write the XKB keymap into an anonymous shared-memory file ──
    let keymap_bytes = keymap_str.as_bytes();
    let keymap_size = (keymap_bytes.len() + 1) as u32; // +1 for null terminator

    let raw_fd = unsafe {
        libc::memfd_create(b"vk-keymap\0".as_ptr() as *const libc::c_char, 0)
    };
    if raw_fd < 0 {
        eprintln!("VK: memfd_create failed: {}", std::io::Error::last_os_error());
        return;
    }

    // Safety: raw_fd is a valid, newly created fd owned by us.
    let mut keymap_file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
    if keymap_file.write_all(keymap_bytes).is_err()
        || keymap_file.write_all(&[0u8]).is_err()
    {
        eprintln!("VK: failed to write keymap to memfd");
        return;
    }

    // WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1 = 1
    // The Wayland backend dups the fd internally before sending.
    vk.keymap(1, keymap_file.as_fd(), keymap_size);

    // Roundtrip so the compositor has loaded and compiled the keymap
    // before our key events arrive.
    if let Err(e) = queue.roundtrip(&mut state) {
        eprintln!("VK: keymap roundtrip failed: {}", e);
        return;
    }
    drop(keymap_file); // fd is no longer needed after the roundtrip

    // ── Queue all key press / release events ──
    // Each character gets 30 ms between events; press duration is 15 ms.
    let base_t = now_ms();
    for (i, c) in text_spaced.chars().enumerate() {
        if let Some(&kc) = char_to_kc.get(&c) {
            let t = base_t.wrapping_add(i as u32 * 30);
            vk.key(t, kc, 1);      // WL_KEYBOARD_KEY_STATE_PRESSED  = 1
            vk.key(t + 15, kc, 0); // WL_KEYBOARD_KEY_STATE_RELEASED = 0
        }
    }

    // Roundtrip after key events: this blocks until the compositor has
    // processed everything in the queue.  A bare flush() returns as soon as
    // the bytes are written to the socket — the compositor may not have read
    // them yet, and dropping the connection immediately after would cause it
    // to discard the in-flight events entirely.
    if let Err(e) = queue.roundtrip(&mut state) {
        eprintln!("VK: final roundtrip failed: {}", e);
    }

    println!("→ Typed via virtual keyboard: {}", text_spaced.trim());
}

// ───────────────────────────────────────────────
// D-BUS OSK CHECK via zbus
// Replaces the gdbus subprocess. The session-bus
// connection is created once and cached for the
// lifetime of the process.
// ───────────────────────────────────────────────

fn get_or_init_zbus() -> Option<ZbusConnection> {
    ZBUS_CONN.with(|cell| {
        let mut opt = cell.borrow_mut();
        if opt.is_none() {
            match ZbusConnection::session() {
                Ok(c) => *opt = Some(c),
                Err(e) => eprintln!("zbus: session connection failed: {}", e),
            }
        }
        // ZbusConnection is Arc-backed, so clone is cheap.
        opt.clone()
    })
}

fn is_osk_visible() -> bool {
    let conn = match get_or_init_zbus() {
        Some(c) => c,
        None => return false,
    };

    let result: Result<bool, Box<dyn std::error::Error>> = (|| {
        use zbus::blocking::fdo::PropertiesProxy;
        let proxy = PropertiesProxy::builder(&conn)
            .destination("sm.puri.OSK0")?
            .path("/sm/puri/OSK0")?
            .build()?;
        let val = proxy.get("sm.puri.OSK0".try_into()?, "Visible")?;
        Ok(bool::try_from(val)?)
    })();

    match result {
        Ok(v) => {
            println!("OSK Visible: {}", v);
            v
        }
        Err(e) => {
            eprintln!("zbus: OSK visibility check failed: {}", e);
            false
        }
    }
}

// ───────────────────────────────────────────────
// DYNAMIC MODEL LOAD / UNLOAD
// ───────────────────────────────────────────────
fn load_vosk_model(state: &mut AudioState, model_path: &str) {
    if state.is_model_loaded {
        return;
    }
    let c_path = match CString::new(model_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Model path invalid: {}", e);
            return;
        }
    };
    unsafe {
        let model = vosk_model_new(c_path.as_ptr() as *const i8);
        if model.is_null() {
            eprintln!("Failed to load Vosk model at {}", model_path);
            return;
        }
        let recognizer = vosk_recognizer_new(model, SAMPLE_RATE as f32);
        if recognizer.is_null() {
            eprintln!("Failed to create Vosk recognizer");
            vosk_model_free(model);
            return;
        }
        state.model = model;
        state.recognizer = recognizer;
        state.is_model_loaded = true;
        println!("Vosk model loaded from: {}", model_path);
    }
}

fn unload_vosk_model(state: &mut AudioState) {
    if !state.is_model_loaded {
        return;
    }
    unsafe {
        if !state.recognizer.is_null() {
            vosk_recognizer_free(state.recognizer);
            state.recognizer = ptr::null_mut();
        }
        if !state.model.is_null() {
            vosk_model_free(state.model);
            state.model = ptr::null_mut();
        }
    }
    state.is_model_loaded = false;
    println!("Vosk model unloaded");
}

// ───────────────────────────────────────────────
// AUDIO ENHANCEMENT
// Uses PulseAudio's module-echo-cancel with the
// WebRTC backend to apply:
//   • Noise suppression
//   • Echo cancellation
//   • Automatic gain control (AGC)
//   • High-pass filter (removes low rumble)
//   • Extended filter for better AEC accuracy
//
// A virtual source named "voskboard_mic_enhanced"
// is created; we record from that instead of the
// raw default source.  The module is unloaded on
// clean shutdown.
// ───────────────────────────────────────────────

const ENHANCED_SOURCE_NAME: &str = "voskboard_mic_enhanced";

/// Load module-echo-cancel.  Returns (module_id, source_name_cstring) on
/// success, or None if the module is unavailable or pactl fails.
fn setup_audio_enhancement() -> Option<(u32, CString)> {
    let output = Command::new("pactl")
        .args([
            "load-module",
            "module-echo-cancel",
            "aec_method=webrtc",
            &format!("source_name={}", ENHANCED_SOURCE_NAME),
            &format!("source_props=device.description=VoskboardMic"),
            // WebRTC AEC parameters:
            //   noise_suppression  — suppress background noise
            //   analog_agc         — hardware-level gain normalisation
            //   digital_agc        — disabled (can distort quiet speech)
            //   extended_filter    — longer echo tail, better accuracy
            //   high_pass_filter   — remove sub-100 Hz rumble
            "aec_args=noise_suppression=true analog_agc=true digital_agc=false \
                      extended_filter=true high_pass_filter=true",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        eprintln!(
            "Audio enhancement: pactl load-module failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return None;
    }

    let module_id: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;

    let source = CString::new(ENHANCED_SOURCE_NAME).ok()?;
    println!("Audio enhancement active — module {} source '{}'", module_id, ENHANCED_SOURCE_NAME);
    Some((module_id, source))
}

/// Unload the echo-cancel module when shutting down.
fn teardown_audio_enhancement(module_id: u32) {
    let status = Command::new("pactl")
        .args(["unload-module", &module_id.to_string()])
        .status();
    match status {
        Ok(s) if s.success() => println!("Audio enhancement module {} unloaded", module_id),
        Ok(s) => eprintln!("Audio enhancement unload exited {:?}", s.code()),
        Err(e) => eprintln!("Audio enhancement unload failed: {}", e),
    }
}

// ───────────────────────────────────────────────
// RECORDING CONTROL
// ───────────────────────────────────────────────
fn start_recording(model_path: &str, config: &Config) {
    AUDIO_STATE.with(|cell| {
        let mut opt = cell.borrow_mut();
        if let Some(state) = opt.as_mut() {
            if config.ram_saving && !state.is_model_loaded {
                load_vosk_model(state, model_path);
            }
            if !state.is_model_loaded {
                eprintln!("Cannot start recording: model not loaded");
                return;
            }
            if state.is_recording {
                println!("Already recording");
                return;
            }
            unsafe {
                if !state.pa_stream.is_null() {
                    pa_stream_disconnect(state.pa_stream);
                    pa_stream_flush(state.pa_stream);
                    pa_stream_unref(state.pa_stream);
                    state.pa_stream = ptr::null_mut();
                }
                let spec = pa_sample_spec { format: PA_SAMPLE_S16LE, rate: SAMPLE_RATE, channels: 1 };
                let stream_name = CString::new("vosk-mic").unwrap();
                let new_stream = pa_stream_new(state.pa_context, stream_name.as_ptr() as *const i8, &spec, ptr::null());
                if new_stream.is_null() {
                    eprintln!("Failed to create new stream");
                    return;
                }
                pa_stream_set_read_callback(new_stream, Some(audio_callback), state.recognizer);
                let device_ptr = state.enhanced_source
                    .as_ref()
                    .map(|s| s.as_ptr() as *const i8)
                    .unwrap_or(ptr::null());
                let ret = pa_stream_connect_record(new_stream, device_ptr, ptr::null(), 0);
                if ret < 0 {
                    let err = if !pa_strerror(ret).is_null() {
                        CStr::from_ptr(pa_strerror(ret) as *const u8).to_string_lossy().into_owned()
                    } else {
                        format!("code {}", ret)
                    };
                    eprintln!("pa_stream_connect_record failed: {}", err);
                    pa_stream_unref(new_stream);
                    return;
                }
                state.pa_stream = new_stream;
                state.is_recording = true;
                println!("Recording started");
            }
        }
    });
}

fn stop_recording(config: &Config) {
    AUDIO_STATE.with(|cell| {
        let mut opt = cell.borrow_mut();
        if let Some(state) = opt.as_mut() {
            if !state.is_recording {
                println!("Not recording");
                return;
            }
            unsafe {
                if !state.pa_stream.is_null() {
                    pa_stream_disconnect(state.pa_stream);
                    pa_stream_flush(state.pa_stream);
                    pa_stream_unref(state.pa_stream);
                    state.pa_stream = ptr::null_mut();
                }
            }
            state.is_recording = false;
            println!("Recording stopped");
            if config.ram_saving {
                unload_vosk_model(state);
            }
        }
    });
}

// ───────────────────────────────────────────────
// GTK UI + FALLBACK TIMER
// ───────────────────────────────────────────────
fn build_ui(app: &Application, model_path: String, config: Config) {
    let window = ApplicationWindow::builder()
        .application(app)
        .default_width(60)
        .default_height(60)
        .visible(false)
        .build();

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_anchor(Edge::Right, true);
    window.set_anchor(Edge::Bottom, true);
    window.set_margin(Edge::Right, 20);
    window.set_margin(Edge::Bottom, 20);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_exclusive_zone(0);
    window.set_decorated(false);
    window.set_resizable(false);

    let btn = Button::new();
    btn.set_label("🎤");

    let is_recording = Rc::new(RefCell::new(false));
    let btn_clone = btn.clone();
    let is_recording_clone = is_recording.clone();
    let model_path_clone = model_path.clone();
    let config_clone = config.clone();

    btn.connect_clicked(move |_| {
        let mut recording = is_recording_clone.borrow_mut();
        if *recording {
            stop_recording(&config_clone);
            btn_clone.set_label("🎤");
            btn_clone.remove_css_class("recording");
        } else {
            start_recording(&model_path_clone, &config_clone);
            btn_clone.set_label("●");
            btn_clone.add_css_class("recording");
        }
        *recording = !*recording;
    });

    let provider = CssProvider::new();
    let css_bytes = Bytes::from_static(
        b"window {
            background: transparent;
            border: none;
        }
        button {
            background: rgba(0, 0, 0, 0.4);
            border-radius: 50%;
            color: white;
            min-width: 50px;
            min-height: 50px;
            padding: 0;
        }
        .recording {
            background: red;
        }"
    );
    provider.load_from_bytes(&css_bytes);
    let display = gtk::gdk::Display::default().expect("No display");
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    window.set_child(Some(&btn));

    if config.always_visible {
        window.present();
        println!("Always visible mode: button shown");
        return;
    }

    let previous_visible = Rc::new(RefCell::new(false));

    let initial_visible = is_osk_visible();
    *previous_visible.borrow_mut() = initial_visible;

    if initial_visible {
        AUDIO_STATE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if let Some(state) = opt.as_mut() {
                if config.ram_saving {
                    load_vosk_model(state, &model_path);
                }
            }
        });
        window.present();
        println!("Initial check: OSK visible → button shown");
    } else {
        println!("Initial check: OSK hidden → button hidden");
    }

    // Fallback timer – poll every 2 seconds via zbus
    let window_clone = window.clone();
    let previous_visible_clone = previous_visible.clone();
    let model_path_clone2 = model_path.clone();
    let config_clone2 = config.clone();

    glib::timeout_add_local(Duration::from_secs(2), move || {
        let visible = is_osk_visible();
        let mut prev = previous_visible_clone.borrow_mut();

        if visible != *prev {
            println!("OSK visibility changed: {} → {}", *prev, visible);

            if visible {
                AUDIO_STATE.with(|cell| {
                    let mut opt = cell.borrow_mut();
                    if let Some(state) = opt.as_mut() {
                        if config_clone2.ram_saving {
                            load_vosk_model(state, &model_path_clone2);
                        }
                    }
                });
                window_clone.present();
                println!("OSK appeared → showing button");
            } else {
                AUDIO_STATE.with(|cell| {
                    let mut opt = cell.borrow_mut();
                    if let Some(state) = opt.as_mut() {
                        if config_clone2.ram_saving {
                            unload_vosk_model(state);
                        }
                    }
                });
                window_clone.set_visible(false);
                println!("OSK disappeared → hiding button");
            }

            *prev = visible;
        }

        ControlFlow::Continue
    });

    println!("Fallback timer started (2s interval)");
}

// ───────────────────────────────────────────────
// CONFIG
// ───────────────────────────────────────────────
fn default_true() -> bool { true }

#[derive(Deserialize, Clone)]
struct Config {
    #[serde(default)]
    model_size: String,
    #[serde(default = "default_true")]
    ram_saving: bool,
    #[serde(default)]
    always_visible: bool,
    /// Capitalise sentence start + (with nlp_correction) proper nouns / German nouns.
    #[serde(default = "default_true")]
    auto_capitalize: bool,
    /// Add terminal '.' or '?' heuristically to each utterance.
    #[serde(default = "default_true")]
    auto_punctuate: bool,
    /// Apply nlprule grammar correction (English and German only).
    #[serde(default = "default_true")]
    nlp_correction: bool,
    /// Load PulseAudio module-echo-cancel (WebRTC noise suppression + AGC).
    #[serde(default)]
    audio_enhancement: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model_size: String::new(),
            ram_saving: true,
            always_visible: false,
            auto_capitalize: true,
            auto_punctuate: true,
            nlp_correction: true,
            audio_enhancement: false,
        }
    }
}

// ───────────────────────────────────────────────
// MAIN
// ───────────────────────────────────────────────
fn main() {
    std::env::set_var("GDK_BACKEND", "wayland");

    let home = env::var("HOME").expect("HOME env var");
    let config_dir = format!("{}/.config/voskboard", home);
    fs::create_dir_all(&config_dir).ok();
    let config_path = format!("{}/config.toml", config_dir);
    let config_str = read_to_string(&config_path).unwrap_or_else(|_| {
        r#"
model_size = "none"
ram_saving = true
always_visible = false
"#
        .to_string()
    });
    let mut config: Config = toml::from_str(&config_str).unwrap_or_default();
    if config.model_size.is_empty() {
        config.model_size = "none".to_string();
    }

    let model_folder = match config.model_size.as_str() {
        "none" => return,
        "small" => "vosk-model-small-en-us-0.15",
        "medium" => "vosk-model-en-us-0.22",
        "large" => "vosk-model-en-us-0.42-gigaspeech",
        "small-german" => "vosk-model-small-de-0.15",
        "small-russian" => "vosk-model-small-ru-0.22",
        "small-dutch" => "vosk-model-small-nl-0.22",
        "small-french" => "vosk-model-small-fr-0.22",
        "small-spanish" => "vosk-model-small-es-0.42",
        "small-italian" => "vosk-model-small-it-0.22",
        "small-swedish" => "vosk-model-small-sv-rhasspy-0.15",
        "small-polish" => "vosk-model-small-pl-0.22",
        "small-korean" => "vosk-model-small-ko-0.22",
        "small-chinese" => "vosk-model-small-cn-0.22",
        "small-japanese" => "vosk-model-small-ja-0.22",
        _ => "vosk-model-small-en-us-0.15",
    };
    let model_path = format!("{}/.local/share/vosk-models/{}", home, model_folder);

    // Derive and store the voice language once — used by punctuation and
    // Harper gating throughout the session.
    let voice_lang = VoiceLang::from_model_size(&config.model_size);
    VOICE_LANG.with(|c| *c.borrow_mut() = voice_lang);
    println!("Voice language: {:?}", voice_lang);

    unsafe {
        let gctx = glib::MainContext::default().as_ptr() as *mut std::ffi::c_void;
        let glib_mainloop = pa_glib_mainloop_new(gctx);
        if glib_mainloop.is_null() {
            panic!("pa_glib_mainloop_new failed");
        }
        let api = pa_glib_mainloop_get_api(glib_mainloop);
        let ctx_name = CString::new("vosk-mic-toggle").unwrap();
        let ctx = pa_context_new(api, ctx_name.as_ptr() as *const i8);
        if ctx.is_null() {
            panic!("pa_context_new failed");
        }
        let ret = pa_context_connect(ctx, ptr::null(), 0, ptr::null_mut());
        if ret < 0 {
            let err = if !pa_strerror(ret).is_null() {
                CStr::from_ptr(pa_strerror(ret) as *const u8)
                    .to_string_lossy()
                    .into_owned()
            } else {
                "unknown error".to_string()
            };
            eprintln!("pa_context_connect failed: {} (code {})", err, ret);
        }

        AUDIO_STATE.set(Some(AudioState {
            recognizer: ptr::null_mut(),
            model: ptr::null_mut(),
            pa_glib_mainloop: glib_mainloop,
            pa_context: ctx,
            pa_stream: ptr::null_mut(),
            is_recording: false,
            is_model_loaded: false,
            echo_cancel_module_id: None,
            enhanced_source: None,
        }));
    }

    if !config.ram_saving && config.model_size != "none" {
        AUDIO_STATE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if let Some(state) = opt.as_mut() {
                load_vosk_model(state, &model_path);
            }
        });
    }

    // Store feature toggles in the thread_local so the C audio callback
    // can read them without needing Rc or config references.
    FEATURE_FLAGS.with(|f| {
        *f.borrow_mut() = FeatureFlags {
            auto_capitalize: config.auto_capitalize,
            auto_punctuate:  config.auto_punctuate,
            nlp_correction:  config.nlp_correction,
        };
    });

    // Audio enhancement — load PulseAudio echo-cancel module if requested.
    if config.audio_enhancement {
        match setup_audio_enhancement() {
            Some((module_id, source)) => {
                AUDIO_STATE.with(|cell| {
                    if let Some(state) = cell.borrow_mut().as_mut() {
                        state.echo_cancel_module_id = Some(module_id);
                        state.enhanced_source = Some(source);
                    }
                });
            }
            None => eprintln!("Audio enhancement requested but unavailable — \
                               is pulseaudio module-echo-cancel installed?"),
        }
    }

    // Pre-load nlprule for English and German only.
    // Both language binaries are baked into the executable at compile time
    // via include_bytes! in build.rs, but only the active language is parsed
    // into RAM here.  All other languages skip this block entirely.
    if voice_lang.uses_nlprule() {
        NLPRULE.with(|cell| {
            let mut opt = cell.borrow_mut();
            let (tokenizer_bytes, rules_bytes): (&'static [u8], &'static [u8]) = match voice_lang {
                VoiceLang::German => (
                    include_bytes!(concat!(env!("OUT_DIR"), "/", tokenizer_filename!("de"))),
                    include_bytes!(concat!(env!("OUT_DIR"), "/", rules_filename!("de"))),
                ),
                _ => (
                    include_bytes!(concat!(env!("OUT_DIR"), "/", tokenizer_filename!("en"))),
                    include_bytes!(concat!(env!("OUT_DIR"), "/", rules_filename!("en"))),
                ),
            };
            match (
                Tokenizer::from_reader(&mut std::io::Cursor::new(tokenizer_bytes)),
                Rules::from_reader(&mut std::io::Cursor::new(rules_bytes)),
            ) {
                (Ok(tokenizer), Ok(rules)) => {
                    *opt = Some((rules, tokenizer));
                    println!("nlprule loaded for {:?}", voice_lang);
                }
                (Err(e), _) | (_, Err(e)) => {
                    eprintln!("nlprule init failed: {}", e);
                }
            }
        });
    }

    let app = Application::builder()
        .application_id("org.example.voskboard")
        .build();

    let model_path_clone = model_path.clone();
    let config_clone = config.clone();
    app.connect_activate(move |app| {
        build_ui(app, model_path_clone.clone(), config_clone.clone());
    });

    // Unload the echo-cancel module on clean shutdown.
    app.connect_shutdown(|_| {
        AUDIO_STATE.with(|cell| {
            if let Some(state) = cell.borrow().as_ref() {
                if let Some(id) = state.echo_cancel_module_id {
                    teardown_audio_enhancement(id);
                }
            }
        });
    });

    app.run();
}
