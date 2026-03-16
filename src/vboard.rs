#![allow(deprecated)]

use gtk::prelude::*;
use gtk::{Application, ApplicationWindow, Box as GtkBox, Button, Label, Orientation, Switch, ProgressBar, Align};
use gtk::ComboBoxText;
use std::rc::Rc;
use std::cell::RefCell;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use serde::{Deserialize, Serialize};
use toml;
use glib::{timeout_add_local, ControlFlow};

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    #[serde(default)]
    model_size: String,
    #[serde(default = "default_true")]
    ram_saving: bool,
    #[serde(default)]
    always_visible: bool,
    #[serde(default)]
    start_on_boot: bool,
    /// Capitalise sentence start + proper nouns (via nlprule for en/de).
    #[serde(default = "default_true")]
    auto_capitalize: bool,
    /// Append terminal '.' or '?' to each utterance.
    #[serde(default = "default_true")]
    auto_punctuate: bool,
    /// Apply nlprule grammar correction (English and German only).
    #[serde(default = "default_true")]
    nlp_correction: bool,
    /// Enable PulseAudio WebRTC noise suppression + AGC on the mic.
    #[serde(default)]
    audio_enhancement: bool,
}

fn default_true() -> bool { true }

impl Default for Config {
    fn default() -> Self {
        Self {
            model_size: "none".to_string(),
            ram_saving: true,
            always_visible: false,
            start_on_boot: false,
            auto_capitalize: true,
            auto_punctuate: true,
            nlp_correction: true,
            audio_enhancement: false,
        }
    }
}

const MODEL_BASE_DIR_SUFFIX: &str = ".local/share/vosk-models";

enum DownloadMsg {
    Progress { downloaded: u64, total: u64 },
    Extracting,
    Done,
    Failed(String),
    Cancelled,
}

fn main() {
    let app = Application::builder()
        .application_id("org.example.voskboard.config")
        .build();

    app.connect_activate(build_ui);
    app.run();
}

fn build_ui(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Vboard Configuration")
        .default_width(480)
        .build();

    // Size the window to fit whatever screen it opens on.
    // On a phone the display is often only 720–800 px tall.
    let screen_height = gtk::gdk::Display::default()
        .and_then(|d| d.monitors().item(0))
        .and_downcast::<gtk::gdk::Monitor>()
        .map(|m| m.geometry().height())
        .unwrap_or(600);
    // Leave a small margin so the window bar / status bar aren't covered.
    let win_height = (screen_height - 60).max(300);
    window.set_default_size(480, win_height);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);

    let main_vbox = GtkBox::new(Orientation::Vertical, 12);
    main_vbox.set_margin_start(24);
    main_vbox.set_margin_end(24);
    main_vbox.set_margin_top(20);
    main_vbox.set_margin_bottom(20);

    let home = std::env::var("HOME").expect("HOME not set");

    let models_base = format!("{}/{}", home, MODEL_BASE_DIR_SUFFIX);
    let _ = fs::create_dir_all(&models_base);

    let config_dir = format!("{}/.config/voskboard", home);
    let _ = fs::create_dir_all(&config_dir);
    let config_path = format!("{}/config.toml", config_dir);

    if !Path::new(&config_path).exists() {
        let default_config = Config::default();
        let _ = fs::write(&config_path, toml::to_string(&default_config).unwrap_or_default());
    }

    let config_str = fs::read_to_string(&config_path).unwrap_or_default();
    let mut config: Config = toml::from_str(&config_str).unwrap_or_default();
    if config.model_size.is_empty() {
        config.model_size = "none".to_string();
    }
    // Migrate: re-save so any fields absent in an older config get written back.
    let _ = fs::write(&config_path, toml::to_string(&config).unwrap_or_default());

    let config_rc = Rc::new(RefCell::new(config));

    // ── Model selector ──────────────────────────────────────────────────────

    let model_row = create_labeled_row("Vosk Model:");
    let model_combo = ComboBoxText::new();
    model_combo.append_text("small");
    model_combo.append_text("medium");
    model_combo.append_text("large");
    model_combo.append_text("small-german");
    model_combo.append_text("small-russian");
    model_combo.append_text("small-dutch");
    model_combo.append_text("small-french");
    model_combo.append_text("small-spanish");
    model_combo.append_text("small-italian");
    model_combo.append_text("small-swedish");
    model_combo.append_text("small-polish");
    model_combo.append_text("small-korean");
    model_combo.append_text("small-chinese");
    model_combo.append_text("small-japanese");
    match config_rc.borrow().model_size.as_str() {
        "small"          => model_combo.set_active(Some(0)),
        "medium"         => model_combo.set_active(Some(1)),
        "large"          => model_combo.set_active(Some(2)),
        "small-german"   => model_combo.set_active(Some(3)),
        "small-russian"  => model_combo.set_active(Some(4)),
        "small-dutch"    => model_combo.set_active(Some(5)),
        "small-french"   => model_combo.set_active(Some(6)),
        "small-spanish"  => model_combo.set_active(Some(7)),
        "small-italian"  => model_combo.set_active(Some(8)),
        "small-swedish"  => model_combo.set_active(Some(9)),
        "small-polish"   => model_combo.set_active(Some(10)),
        "small-korean"   => model_combo.set_active(Some(11)),
        "small-chinese"  => model_combo.set_active(Some(12)),
        "small-japanese" => model_combo.set_active(Some(13)),
        _                => model_combo.set_active(None),
    };
    model_row.append(&model_combo);

    let download_btn = Button::with_label("Download & Set Model");
    download_btn.set_halign(Align::End);
    let download_row = GtkBox::new(Orientation::Horizontal, 0);
    download_row.append(&download_btn);

    let progress_vbox = GtkBox::new(Orientation::Vertical, 6);
    progress_vbox.set_visible(false);

    let progress_bar = ProgressBar::new();
    progress_bar.set_show_text(true);
    progress_bar.set_fraction(0.0);
    progress_bar.set_hexpand(true);

    let cancel_btn = Button::with_label("Cancel");
    cancel_btn.set_halign(Align::Center);
    cancel_btn.set_sensitive(false);

    progress_vbox.append(&progress_bar);
    progress_vbox.append(&cancel_btn);

    let status_label = Label::new(Some(""));
    status_label.set_halign(Align::Center);

    // ── General settings ────────────────────────────────────────────────────
    let ram_switch = Switch::new();
    ram_switch.set_active(config_rc.borrow().ram_saving);
    let ram_row = create_switch_row("RAM Saving Mode", &ram_switch);

    let visible_switch = Switch::new();
    visible_switch.set_active(config_rc.borrow().always_visible);
    let visible_row = create_switch_row("Always Show Button", &visible_switch);

    let auto_switch = Switch::new();
    let autostart_path = format!("{}/.config/autostart/voskboard.desktop", home);
    let boot_active = config_rc.borrow().start_on_boot || Path::new(&autostart_path).exists();
    auto_switch.set_active(boot_active);
    let auto_row = create_switch_row("Start on Boot", &auto_switch);

    // ── Text processing settings ─────────────────────────────────────────────
    let capitalize_switch = Switch::new();
    capitalize_switch.set_active(config_rc.borrow().auto_capitalize);
    let capitalize_row = create_switch_row(
        "Auto Capitalise  (sentence start)",
        &capitalize_switch,
    );

    let punctuate_switch = Switch::new();
    punctuate_switch.set_active(config_rc.borrow().auto_punctuate);
    let punctuate_row = create_switch_row(
        "Auto Punctuate  (adds . or ?)",
        &punctuate_switch,
    );

    let nlp_switch = Switch::new();
    nlp_switch.set_active(config_rc.borrow().nlp_correction);
    let nlp_row = create_switch_row(
        "NLP Grammar Correction  (English & German only)",
        &nlp_switch,
    );

    // ── Audio settings ───────────────────────────────────────────────────────
    let audio_switch = Switch::new();
    audio_switch.set_active(config_rc.borrow().audio_enhancement);
    let audio_row = create_switch_row(
        "Enhance Mic  (noise supp + AGC, more ram used)",
        &audio_switch,
    );

    let service_btn = Button::with_label(get_service_button_label());
    service_btn.set_size_request(220, -1);
    service_btn.set_halign(Align::Center);

    main_vbox.append(&model_row);
    main_vbox.append(&download_row);
    main_vbox.append(&progress_vbox);
    main_vbox.append(&status_label);
    main_vbox.append(&ram_row);
    main_vbox.append(&visible_row);
    main_vbox.append(&auto_row);
    main_vbox.append(&capitalize_row);
    main_vbox.append(&punctuate_row);
    main_vbox.append(&nlp_row);
    main_vbox.append(&audio_row);
    main_vbox.append(&service_btn);

    scroll.set_child(Some(&main_vbox));
    window.set_child(Some(&scroll));

    // ── Switch save handlers ────────────────────────────────────────────────
    {
        let config_rc = config_rc.clone(); let config_path = config_path.clone();
        ram_switch.connect_state_set(move |_, state| {
            config_rc.borrow_mut().ram_saving = state;
            save_config(&config_path, &config_rc.borrow());
            false.into()
        });
    }
    {
        let config_rc = config_rc.clone(); let config_path = config_path.clone();
        visible_switch.connect_state_set(move |_, state| {
            config_rc.borrow_mut().always_visible = state;
            save_config(&config_path, &config_rc.borrow());
            false.into()
        });
    }
    {
        let config_rc = config_rc.clone(); let config_path = config_path.clone();
        let autostart_path = autostart_path.clone();
        auto_switch.connect_state_set(move |_, state| {
            config_rc.borrow_mut().start_on_boot = state;
            save_config(&config_path, &config_rc.borrow());
            if state {
                if let Err(e) = create_autostart_file(&autostart_path) {
                    eprintln!("Failed to create autostart file: {}", e);
                }
            } else {
                let _ = fs::remove_file(&autostart_path);
            }
            false.into()
        });
    }
    {
        let config_rc = config_rc.clone(); let config_path = config_path.clone();
        capitalize_switch.connect_state_set(move |_, state| {
            config_rc.borrow_mut().auto_capitalize = state;
            save_config(&config_path, &config_rc.borrow());
            false.into()
        });
    }
    {
        let config_rc = config_rc.clone(); let config_path = config_path.clone();
        punctuate_switch.connect_state_set(move |_, state| {
            config_rc.borrow_mut().auto_punctuate = state;
            save_config(&config_path, &config_rc.borrow());
            false.into()
        });
    }
    {
        let config_rc = config_rc.clone(); let config_path = config_path.clone();
        nlp_switch.connect_state_set(move |_, state| {
            config_rc.borrow_mut().nlp_correction = state;
            save_config(&config_path, &config_rc.borrow());
            false.into()
        });
    }
    {
        let config_rc = config_rc.clone(); let config_path = config_path.clone();
        audio_switch.connect_state_set(move |_, state| {
            config_rc.borrow_mut().audio_enhancement = state;
            save_config(&config_path, &config_rc.borrow());
            false.into()
        });
    }

    let cancel_flag: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // ── Download button ─────────────────────────────────────────────────────
    {
        let download_btn  = download_btn.clone();
        let progress_vbox = progress_vbox.clone();
        let progress_bar  = progress_bar.clone();
        let status_label  = status_label.clone();
        let cancel_btn    = cancel_btn.clone();
        let model_combo   = model_combo.clone();
        let config_rc     = config_rc.clone();
        let config_path   = config_path.clone();
        let cancel_flag   = cancel_flag.clone();
        let home          = home.clone();

        download_btn.clone().connect_clicked(move |_| {
            let Some(text) = model_combo.active_text() else {
                status_label.set_text("Please select a model first");
                return;
            };
            let new_size = text.to_string();
            let current  = config_rc.borrow().model_size.clone();

            if new_size == current {
                status_label.set_text("Already using this model");
                return;
            }

            let base = "https://alphacephei.com/vosk/models";
            let (url, folder): (String, &str) = match new_size.as_str() {
                "small"          => (format!("{}/vosk-model-small-en-us-0.15.zip", base),          "vosk-model-small-en-us-0.15"),
                "medium"         => (format!("{}/vosk-model-en-us-0.22.zip", base),                 "vosk-model-en-us-0.22"),
                "large"          => (format!("{}/vosk-model-en-us-0.42-gigaspeech.zip", base),      "vosk-model-en-us-0.42-gigaspeech"),
                "small-german"   => (format!("{}/vosk-model-small-de-0.15.zip", base),              "vosk-model-small-de-0.15"),
                "small-russian"  => (format!("{}/vosk-model-small-ru-0.22.zip", base),              "vosk-model-small-ru-0.22"),
                "small-dutch"    => (format!("{}/vosk-model-small-nl-0.22.zip", base),              "vosk-model-small-nl-0.22"),
                "small-french"   => (format!("{}/vosk-model-small-fr-0.22.zip", base),              "vosk-model-small-fr-0.22"),
                "small-spanish"  => (format!("{}/vosk-model-small-es-0.42.zip", base),              "vosk-model-small-es-0.42"),
                "small-italian"  => (format!("{}/vosk-model-small-it-0.22.zip", base),              "vosk-model-small-it-0.22"),
                "small-swedish"  => (format!("{}/vosk-model-small-sv-rhasspy-0.15.zip", base),      "vosk-model-small-sv-rhasspy-0.15"),
                "small-polish"   => (format!("{}/vosk-model-small-pl-0.22.zip", base),              "vosk-model-small-pl-0.22"),
                "small-korean"   => (format!("{}/vosk-model-small-ko-0.22.zip", base),              "vosk-model-small-ko-0.22"),
                "small-chinese"  => (format!("{}/vosk-model-small-cn-0.22.zip", base),              "vosk-model-small-cn-0.22"),
                "small-japanese" => (format!("{}/vosk-model-small-ja-0.22.zip", base),              "vosk-model-small-ja-0.22"),
                _ => { status_label.set_text("Invalid model selection"); return; }
            };

            cancel_flag.store(false, Ordering::SeqCst);
            download_btn.set_sensitive(false);
            download_btn.set_label("Downloading...");
            cancel_btn.set_sensitive(true);
            progress_vbox.set_visible(true);
            progress_bar.set_fraction(0.0);
            progress_bar.set_text(Some("0%"));
            status_label.set_text("Starting download...");

            let (tx, rx) = mpsc::channel::<DownloadMsg>();

            let rx_cell       = Rc::new(RefCell::new(Some(rx)));
            let poll_bar      = progress_bar.clone();
            let poll_status   = status_label.clone();
            let poll_btn      = download_btn.clone();
            let poll_cancel   = cancel_btn.clone();
            let poll_vbox     = progress_vbox.clone();
            let poll_config   = config_rc.clone();
            let poll_path     = config_path.clone();
            let poll_new_size = new_size.clone();

            timeout_add_local(Duration::from_millis(300), move || {
                let mut done = false;
                if let Some(rx) = rx_cell.borrow().as_ref() {
                    loop {
                        match rx.try_recv() {
                            Ok(DownloadMsg::Progress { downloaded, total }) => {
                                let fraction = if total > 0 { downloaded as f64 / total as f64 } else { 0.0 };
                                poll_bar.set_fraction(fraction);
                                let pct    = (fraction * 100.0) as u32;
                                let dl_mb  = downloaded as f64 / 1_048_576.0;
                                let tot_mb = total      as f64 / 1_048_576.0;
                                poll_bar.set_text(Some(&format!("{}%  ({:.1} / {:.1} MB)", pct, dl_mb, tot_mb)));
                            }
                            Ok(DownloadMsg::Extracting) => {
                                poll_bar.set_text(Some("Extracting..."));
                                poll_status.set_text("Extracting model...");
                            }
                            Ok(DownloadMsg::Done) => {
                                poll_bar.set_fraction(1.0);
                                poll_bar.set_text(Some("100% - Done"));
                                poll_status.set_text("Model installed successfully!");
                                poll_config.borrow_mut().model_size = poll_new_size.clone();
                                save_config(&poll_path, &poll_config.borrow());
                                poll_btn.set_sensitive(true);
                                poll_btn.set_label("Download & Set Model");
                                poll_cancel.set_sensitive(false);
                                let pv = poll_vbox.clone();
                                timeout_add_local(Duration::from_secs(4), move || {
                                    pv.set_visible(false);
                                    ControlFlow::Break
                                });
                                done = true;
                                break;
                            }
                            Ok(DownloadMsg::Failed(reason)) => {
                                poll_bar.set_text(Some("Failed"));
                                poll_status.set_text(&format!("Error: {}", reason));
                                poll_btn.set_sensitive(true);
                                poll_btn.set_label("Download & Set Model");
                                poll_cancel.set_sensitive(false);
                                let pv = poll_vbox.clone();
                                timeout_add_local(Duration::from_secs(6), move || {
                                    pv.set_visible(false);
                                    ControlFlow::Break
                                });
                                done = true;
                                break;
                            }
                            Ok(DownloadMsg::Cancelled) => {
                                poll_bar.set_text(Some("Cancelled"));
                                poll_status.set_text("Download cancelled");
                                poll_btn.set_sensitive(true);
                                poll_btn.set_label("Download & Set Model");
                                poll_cancel.set_sensitive(false);
                                poll_vbox.set_visible(false);
                                done = true;
                                break;
                            }
                            Err(mpsc::TryRecvError::Empty)        => break,
                            Err(mpsc::TryRecvError::Disconnected) => { done = true; break; }
                        }
                    }
                }
                if done { rx_cell.borrow_mut().take(); ControlFlow::Break }
                else    { ControlFlow::Continue }
            });

            {
                let flag = cancel_flag.clone();
                cancel_btn.connect_clicked(move |_| { flag.store(true, Ordering::SeqCst); });
            }

            let url        = url.to_string();
            let folder     = folder.to_string();
            let models_dir = format!("{}/{}", home, MODEL_BASE_DIR_SUFFIX);
            let zip_path   = format!("{}/{}.zip", models_dir, folder);
            let flag       = cancel_flag.clone();

            let old_folder = match current.as_str() {
                "small"          => "vosk-model-small-en-us-0.15",
                "medium"         => "vosk-model-en-us-0.22",
                "large"          => "vosk-model-en-us-0.42-gigaspeech",
                "small-german"   => "vosk-model-small-de-0.15",
                "small-russian"  => "vosk-model-small-ru-0.22",
                "small-dutch"    => "vosk-model-small-nl-0.22",
                "small-french"   => "vosk-model-small-fr-0.22",
                "small-spanish"  => "vosk-model-small-es-0.42",
                "small-italian"  => "vosk-model-small-it-0.22",
                "small-swedish"  => "vosk-model-small-sv-rhasspy-0.15",
                "small-polish"   => "vosk-model-small-pl-0.22",
                "small-korean"   => "vosk-model-small-ko-0.22",
                "small-chinese"  => "vosk-model-small-cn-0.22",
                "small-japanese" => "vosk-model-small-ja-0.22",
                _                => "",
            };
            if !old_folder.is_empty() {
                let old_path = format!("{}/{}", models_dir, old_folder);
                let _ = fs::remove_dir_all(&old_path);
            }

            std::thread::spawn(move || {
                if flag.load(Ordering::SeqCst) {
                    let _ = tx.send(DownloadMsg::Cancelled);
                    return;
                }

                let total_size: u64 = {
                    let out = Command::new("curl").args(["-sI", &url]).output();
                    match out {
                        Ok(o) => String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .find(|l| l.to_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse::<u64>().ok())
                            .unwrap_or(0),
                        Err(_) => 0,
                    }
                };

                let mut wget = match Command::new("wget")
                    .arg(&url).arg("-O").arg(&zip_path)
                    .stdout(Stdio::null()).stderr(Stdio::null())
                    .spawn()
                {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(DownloadMsg::Failed(format!("Failed to start wget: {}", e)));
                        return;
                    }
                };

                loop {
                    if flag.load(Ordering::SeqCst) {
                        let _ = wget.kill();
                        let _ = fs::remove_file(&zip_path);
                        let _ = tx.send(DownloadMsg::Cancelled);
                        return;
                    }

                    let downloaded = fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
                    let _ = tx.send(DownloadMsg::Progress { downloaded, total: total_size });

                    match wget.try_wait() {
                        Ok(Some(status)) => {
                            if !status.success() {
                                let _ = tx.send(DownloadMsg::Failed(
                                    format!("wget failed (code {:?})", status.code())
                                ));
                                return;
                            }
                            break;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let _ = tx.send(DownloadMsg::Failed(format!("wget wait error: {}", e)));
                            return;
                        }
                    }

                    std::thread::sleep(Duration::from_millis(500));
                }

                let _ = tx.send(DownloadMsg::Extracting);
                let unzip_ok = Command::new("unzip")
                    .args(["-o", &zip_path, "-d", &models_dir])
                    .stdout(Stdio::null()).stderr(Stdio::null())
                    .status()
                    .map_or(false, |s| s.success());

                let _ = fs::remove_file(&zip_path);

                if unzip_ok {
                    let _ = tx.send(DownloadMsg::Done);
                } else {
                    let _ = tx.send(DownloadMsg::Failed(
                        "Extraction failed — is unzip installed?".into()
                    ));
                }
            });
        });
    }

    // ── Service button ──────────────────────────────────────────────────────
    {
        let service_btn_c = service_btn.clone();
        service_btn.connect_clicked(move |_| {
            if is_voskboard_running() {
                let pids_out = Command::new("pgrep").args(["-x", "voskboard"]).output();
                if let Ok(o) = pids_out {
                    let pids: Vec<String> = String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect();
                    for pid in &pids {
                        let _ = Command::new("kill").args(["-9", pid]).output();
                    }
                }
            } else {
                let _ = Command::new("setsid")
                    .arg("voskboard")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .stdin(Stdio::null())
                    .spawn();
            }
            let btn      = service_btn_c.clone();
            let attempts = Rc::new(RefCell::new(0u32));
            timeout_add_local(Duration::from_millis(400), move || {
                let running = is_voskboard_running();
                let attempt = { let mut a = attempts.borrow_mut(); *a += 1; *a };
                btn.set_label(if running { "Stop Service" } else { "Start Service" });
                if attempt >= 10 { ControlFlow::Break } else { ControlFlow::Continue }
            });
        });
    }

    window.present();
}

fn create_labeled_row(text: &str) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 16);
    let label = Label::new(Some(text));
    label.set_halign(Align::Start);
    label.set_hexpand(true);
    row.append(&label);
    row
}

fn create_switch_row(text: &str, switch: &Switch) -> GtkBox {
    let row = create_labeled_row(text);
    switch.set_halign(Align::End);
    row.append(switch);
    row
}

fn save_config(path: &str, config: &Config) {
    let _ = fs::write(path, toml::to_string(config).unwrap_or_default());
}

fn create_autostart_file(path: &str) -> io::Result<()> {
    if let Some(p) = Path::new(path).parent() {
        fs::create_dir_all(p)?;
    }
    let mut f = fs::File::create(path)?;
    f.write_all(b"[Desktop Entry]\nType=Application\nExec=voskboard\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true\nName=Voskboard\n")?;
    Ok(())
}

fn is_voskboard_running() -> bool {
    let out = Command::new("pgrep").args(["-x", "voskboard"]).output();
    match out {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let live_pids: Vec<&str> = stdout.lines()
                .map(|l| l.trim())
                .filter(|pid| {
                    if pid.is_empty() { return false; }
                    let status_path = format!("/proc/{}/status", pid);
                    let state = fs::read_to_string(&status_path).unwrap_or_default();
                    !state.lines()
                        .find(|l| l.starts_with("State:"))
                        .map(|l| l.contains('Z'))
                        .unwrap_or(false)
                })
                .collect();
            !live_pids.is_empty()
        }
        Err(_) => false,
    }
}

fn get_service_button_label() -> &'static str {
    if is_voskboard_running() { "Stop Service" } else { "Start Service" }
}
