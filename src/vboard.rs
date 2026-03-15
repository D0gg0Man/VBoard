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

#[derive(Serialize, Deserialize, Clone, Default)]
struct Config {
    model_size: String,
    ram_saving: bool,
    always_visible: bool,
    start_on_boot: bool,
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
        .default_height(500)
        .build();

    let main_vbox = GtkBox::new(Orientation::Vertical, 12);
    main_vbox.set_margin_start(24);
    main_vbox.set_margin_end(24);
    main_vbox.set_margin_top(20);
    main_vbox.set_margin_bottom(20);

    let home = std::env::var("HOME").expect("HOME not set");

    let models_base = format!("{}/{}", home, MODEL_BASE_DIR_SUFFIX);
    let _ = fs::create_dir_all(&models_base);
    println!("DEBUG: models dir = {}", models_base);

    let config_dir = format!("{}/.config/voskboard", home);
    let _ = fs::create_dir_all(&config_dir);
    let config_path = format!("{}/config.toml", config_dir);

    // Write a default config if none exists yet
    if !Path::new(&config_path).exists() {
        let default_config = Config {
            model_size: "none".to_string(),
            ram_saving: true,
            always_visible: false,
            start_on_boot: false,
        };
        let _ = fs::write(&config_path, toml::to_string(&default_config).unwrap_or_default());
        println!("DEBUG: wrote default config to {}", config_path);
    }

    let config_str = fs::read_to_string(&config_path).unwrap_or_default();
    let mut config: Config = toml::from_str(&config_str).unwrap_or_default();
    if config.model_size.is_empty() {
        config.model_size = "none".to_string();
    }
    // Migrate: re-save so any fields absent in an older config get written back as defaults
    let _ = fs::write(&config_path, toml::to_string(&config).unwrap_or_default());
    println!("DEBUG: loaded config — model={} ram_saving={} always_visible={} start_on_boot={}",
        config.model_size, config.ram_saving, config.always_visible, config.start_on_boot);

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
        "small"         => model_combo.set_active(Some(0)),
        "medium"        => model_combo.set_active(Some(1)),
        "large"         => model_combo.set_active(Some(2)),
        "small-german"  => model_combo.set_active(Some(3)),
        "small-russian" => model_combo.set_active(Some(4)),
        "small-dutch"   => model_combo.set_active(Some(5)),
        "small-french"  => model_combo.set_active(Some(6)),
        "small-spanish" => model_combo.set_active(Some(7)),
        "small-italian" => model_combo.set_active(Some(8)),
        "small-swedish" => model_combo.set_active(Some(9)),
        "small-polish"  => model_combo.set_active(Some(10)),
        "small-korean"  => model_combo.set_active(Some(11)),
        "small-chinese" => model_combo.set_active(Some(12)),
        "small-japanese"=> model_combo.set_active(Some(13)),
        _               => model_combo.set_active(None),
    };
    model_row.append(&model_combo);

    let download_btn = Button::with_label("Download & Set Model");
    download_btn.set_halign(Align::End);
    let download_row = GtkBox::new(Orientation::Horizontal, 0);
    download_row.append(&download_btn);

    // Progress area: bar on top, cancel below
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

    // ── Toggles ─────────────────────────────────────────────────────────────
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
    main_vbox.append(&service_btn);

    window.set_child(Some(&main_vbox));
    println!("DEBUG: UI built");

    // ── Switch save handlers ────────────────────────────────────────────────
    {
        let config_rc   = config_rc.clone();
        let config_path = config_path.clone();
        ram_switch.connect_state_set(move |_, state| {
            println!("DEBUG: ram_saving -> {}", state);
            config_rc.borrow_mut().ram_saving = state;
            save_config(&config_path, &config_rc.borrow());
            false.into() // let GTK update the switch visually
        });
    }
    {
        let config_rc   = config_rc.clone();
        let config_path = config_path.clone();
        visible_switch.connect_state_set(move |_, state| {
            println!("DEBUG: always_visible -> {}", state);
            config_rc.borrow_mut().always_visible = state;
            save_config(&config_path, &config_rc.borrow());
            false.into()
        });
    }
    {
        let config_rc      = config_rc.clone();
        let config_path    = config_path.clone();
        let autostart_path = autostart_path.clone();
        auto_switch.connect_state_set(move |_, state| {
            println!("DEBUG: start_on_boot -> {}", state);
            config_rc.borrow_mut().start_on_boot = state;
            save_config(&config_path, &config_rc.borrow());
            if state {
                if let Err(e) = create_autostart_file(&autostart_path) {
                    eprintln!("DEBUG: failed to create autostart file: {}", e);
                } else {
                    println!("DEBUG: autostart file created at {}", autostart_path);
                }
            } else {
                let _ = fs::remove_file(&autostart_path);
                println!("DEBUG: autostart file removed");
            }
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
            println!("DEBUG: Download button clicked");

            let Some(text) = model_combo.active_text() else {
                status_label.set_text("Please select a model first");
                return;
            };
            let new_size = text.to_string();
            let current  = config_rc.borrow().model_size.clone();
            println!("DEBUG: selected={} current={}", new_size, current);

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

            // ── Poll receiver + update bar every 300 ms on the main thread ──
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
                                println!("DEBUG: progress {}% ({}/{})", pct, downloaded, total);
                            }
                            Ok(DownloadMsg::Extracting) => {
                                poll_bar.set_text(Some("Extracting..."));
                                poll_status.set_text("Extracting model...");
                                println!("DEBUG: extracting");
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
                                println!("DEBUG: done");
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
                                println!("DEBUG: failed: {}", reason);
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
                                println!("DEBUG: cancelled");
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

            // Cancel button
            {
                let flag = cancel_flag.clone();
                cancel_btn.connect_clicked(move |_| {
                    println!("DEBUG: cancel clicked");
                    flag.store(true, Ordering::SeqCst);
                });
            }

            // ── Worker thread ──────────────────────────────────────────────
            // Download zip and extract entirely within the user home directory — no root needed
            let url        = url.to_string();
            let folder     = folder.to_string();
            // Zip lands in /tmp so wget can write it without elevated privileges
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
                println!("DEBUG: removing old model {}", old_path);
                let _ = fs::remove_dir_all(&old_path);
            }

            std::thread::spawn(move || {
                if flag.load(Ordering::SeqCst) {
                    let _ = tx.send(DownloadMsg::Cancelled);
                    return;
                }

                // Step 1: HEAD request to get total size
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
                println!("DEBUG: total_size={} bytes", total_size);

                // Step 2: spawn wget — writes to /tmp, no root needed
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
                println!("DEBUG: wget spawned -> {}", zip_path);

                // Step 3: poll partial file size every 500 ms
                loop {
                    if flag.load(Ordering::SeqCst) {
                        println!("DEBUG: cancel flag — killing wget");
                        let _ = wget.kill();
                        let _ = fs::remove_file(&zip_path);
                        let _ = tx.send(DownloadMsg::Cancelled);
                        return;
                    }

                    let downloaded = fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
                    let _ = tx.send(DownloadMsg::Progress { downloaded, total: total_size });

                    match wget.try_wait() {
                        Ok(Some(status)) => {
                            println!("DEBUG: wget exited {:?}", status);
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

                // Step 4: unzip into ~/.local/share/vosk-models/ — no root needed
                let _ = tx.send(DownloadMsg::Extracting);
                println!("DEBUG: starting unzip into {}", models_dir);
                let unzip_ok = Command::new("unzip")
                    .args(["-o", &zip_path, "-d", &models_dir])
                    .stdout(Stdio::null()).stderr(Stdio::null())
                    .status()
                    .map_or(false, |s| s.success());

                // Step 5: delete the zip from /tmp (no root needed)
                let _ = fs::remove_file(&zip_path);
                println!("DEBUG: zip removed, unzip ok={}", unzip_ok);

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
            println!("DEBUG: service button clicked");
            if is_voskboard_running() {
                println!("DEBUG: stopping voskboard");
                let pids_out = Command::new("pgrep")
                    .args(["-x", "voskboard"])
                    .output();
                if let Ok(o) = pids_out {
                    let pids: Vec<String> = String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect();
                    println!("DEBUG: pids to kill: {:?}", pids);
                    for pid in &pids {
                        let result = Command::new("kill")
                            .args(["-9", pid])
                            .output();
                        match result {
                            Ok(o) => println!("DEBUG: kill -9 {} exit={} stderr={:?}", pid, o.status, String::from_utf8_lossy(&o.stderr).trim()),
                            Err(e) => println!("DEBUG: kill -9 {} error: {}", pid, e),
                        }
                    }
                }
            } else {
                println!("DEBUG: starting voskboard");
                // setsid detaches voskboard into its own session so vboard
                // is not its parent — init adopts it and reaps it on exit
                let _ = Command::new("setsid")
                    .arg("voskboard")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .stdin(Stdio::null())
                    .spawn();
            }
            // Poll every 400 ms up to 10 times (4 sec) until the process state
            // matches what we expect, then update the label. This handles slow
            // start and slow stop without getting stuck.
            let btn        = service_btn_c.clone();
            let attempts   = Rc::new(RefCell::new(0u32));
            timeout_add_local(Duration::from_millis(400), move || {
                let running = is_voskboard_running();
                let attempt = {
                    let mut a = attempts.borrow_mut();
                    *a += 1;
                    *a
                };
                println!("DEBUG: poll {} — running={}", attempt, running);
                btn.set_label(if running { "Stop Service" } else { "Start Service" });
                // Keep polling until state is stable or we've tried 10 times
                if attempt >= 10 {
                    ControlFlow::Break
                } else {
                    ControlFlow::Continue
                }
            });
        });
    }

    window.present();
    println!("DEBUG: window presented");
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
    println!("DEBUG: saving config to {}", path);
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
    let out = Command::new("pgrep")
        .args(["-x", "voskboard"])
        .output();
    match out {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // Filter out zombie processes — they still appear in pgrep after kill
            // until the parent reaps them, but they are not actually running
            let live_pids: Vec<&str> = stdout.lines()
                .map(|l| l.trim())
                .filter(|pid| {
                    if pid.is_empty() { return false; }
                    let status_path = format!("/proc/{}/status", pid);
                    let state = fs::read_to_string(&status_path)
                        .unwrap_or_default();
                    let is_zombie = state.lines()
                        .find(|l| l.starts_with("State:"))
                        .map(|l| l.contains('Z'))
                        .unwrap_or(false);
                    if is_zombie {
                        println!("DEBUG: pid {} is a zombie, ignoring", pid);
                    }
                    !is_zombie
                })
                .collect();
            let running = !live_pids.is_empty();
            println!("DEBUG: pgrep -x voskboard -> running={} live_pids={:?}", running, live_pids);
            running
        }
        Err(e) => {
            println!("DEBUG: pgrep failed: {}", e);
            false
        }
    }
}

fn get_service_button_label() -> &'static str {
    if is_voskboard_running() { "Stop Service" } else { "Start Service" }
}
