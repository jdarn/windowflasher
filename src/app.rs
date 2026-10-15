//! Application state and key handling.

use crate::devices::{self, Device};
use crate::installer::{self, Job, Msg};
use crate::sys::{self, MountSet};
use anyhow::{anyhow, bail, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

pub const DOWNLOAD_URL: &str = "https://www.microsoft.com/en-us/software-download/windows11";

pub struct InstallState {
    pub device: Device,
    pub stage: usize,
    pub progress: f64,
    pub status: String,
    pub log: Vec<String>,
    rx: Receiver<Msg>,
}

pub enum Screen {
    NoIso { note: Option<String> },
    PickIso { isos: Vec<PathBuf>, state: ListState },
    Devices { devices: Vec<Device>, state: ListState, note: Option<String>, why: Vec<String> },
    Confirm { device: Device, yes: bool },
    Installing(InstallState),
    Done { ok: bool, message: String, log: Vec<String> },
}

pub struct App {
    pub screen: Screen,
    pub cwd: PathBuf,
    pub iso_name: Option<String>,
    pub quit: bool,
    /// Also list USB disks that might be SSD/HDD (toggled with A).
    pub show_all: bool,
    iso_mounts: MountSet,
    iso_dir: PathBuf,
}

enum Action {
    None,
    Quit,
    RescanIso,
    UseIso(PathBuf),
    RescanDevices,
    ToggleAll,
    AskConfirm(Device),
    Back,
    Start(Device),
}

fn move_sel(state: &mut ListState, len: usize, delta: i32) {
    if len == 0 {
        return;
    }
    let cur = state.selected().unwrap_or(0) as i32;
    state.select(Some((cur + delta).rem_euclid(len as i32) as usize));
}

impl App {
    pub fn new() -> Result<App> {
        let cwd = std::env::current_dir()?;
        let iso_dir = cwd.join("iso");
        let mut app = App {
            screen: Screen::NoIso { note: None },
            cwd,
            iso_name: None,
            quit: false,
            show_all: false,
            iso_mounts: MountSet::new(),
            iso_dir,
        };
        app.scan_isos();
        Ok(app)
    }

    // ---- ISO handling ----------------------------------------------------

    fn scan_isos(&mut self) {
        let mut isos: Vec<PathBuf> = fs::read_dir(&self.cwd)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_file()
                            && p.extension().map_or(false, |e| e.eq_ignore_ascii_case("iso"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        isos.sort();
        match isos.len() {
            0 => self.screen = Screen::NoIso { note: None },
            1 => {
                let iso = isos.remove(0);
                self.use_iso(iso);
            }
            _ => {
                let mut state = ListState::default();
                state.select(Some(0));
                self.screen = Screen::PickIso { isos, state };
            }
        }
    }

    fn use_iso(&mut self, iso: PathBuf) {
        let name = iso
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.iso_mounts.unmount_all();
        let target = self.iso_dir.clone();
        match self.try_mount_iso(&iso, &target) {
            Ok(()) => {
                self.iso_name = Some(name);
                self.to_devices(None);
            }
            Err(e) => {
                self.iso_mounts.unmount_all();
                self.iso_name = None;
                self.screen = Screen::NoIso { note: Some(format!("{name}: {e:#}")) };
            }
        }
    }

    fn try_mount_iso(&mut self, iso: &Path, target: &Path) -> Result<()> {
        // Windows ISOs contain a stub ISO 9660 view; the real files are in UDF.
        self.iso_mounts.mount(iso, target, "udf", "loop,ro")?;
        let sources = sys::find_ci(target, "sources").ok_or_else(|| {
            anyhow!("no sources folder: this doesn't look like a Windows installer ISO")
        })?;
        if sys::find_ci(&sources, "boot.wim").is_none() {
            bail!("sources/boot.wim is missing: not a Windows installer ISO");
        }
        if sys::find_ci(&sources, "install.wim").is_none()
            && sys::find_ci(&sources, "install.esd").is_none()
        {
            bail!("sources/install.wim (or install.esd) is missing");
        }
        Ok(())
    }

    // ---- Devices ---------------------------------------------------------

    fn to_devices(&mut self, note: Option<String>) {
        match devices::scan(self.show_all) {
            Ok(scan) => {
                let (devices, why) = (scan.devices, scan.why);
                let mut state = ListState::default();
                if !devices.is_empty() {
                    state.select(Some(0));
                }
                self.screen = Screen::Devices { devices, state, note, why };
            }
            Err(e) => {
                self.screen = Screen::Devices {
                    devices: vec![],
                    state: ListState::default(),
                    note: Some(format!("Device scan failed: {e:#}")),
                    why: vec![],
                };
            }
        }
    }

    fn start(&mut self, d: Device) {
        // Re-check that the drive is still there and unchanged before wiping it.
        let still_there = devices::scan(self.show_all)
            .map(|s| s.devices.iter().any(|x| x.path == d.path && x.size == d.size))
            .unwrap_or(false);
        if !still_there {
            self.to_devices(Some("The selected drive is no longer available.".into()));
            return;
        }
        let (tx, rx) = mpsc::channel();
        installer::spawn(
            Job {
                device: d.clone(),
                iso_dir: self.iso_dir.clone(),
                work_dir: self.cwd.clone(),
            },
            tx,
        );
        self.screen = Screen::Installing(InstallState {
            device: d,
            stage: 0,
            progress: 0.0,
            status: String::new(),
            log: Vec::new(),
            rx,
        });
    }

    // ---- Input -----------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl_c {
            // Never abort mid-write: that would leave a half-written drive and mounts behind.
            if !matches!(self.screen, Screen::Installing(_)) {
                self.quit = true;
            }
            return;
        }

        let action = match &mut self.screen {
            Screen::NoIso { .. } => match key.code {
                KeyCode::Char('r') | KeyCode::Char('R') | KeyCode::Enter => Action::RescanIso,
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => Action::Quit,
                _ => Action::None,
            },
            Screen::PickIso { isos, state } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    move_sel(state, isos.len(), -1);
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    move_sel(state, isos.len(), 1);
                    Action::None
                }
                KeyCode::Enter => state
                    .selected()
                    .map(|i| Action::UseIso(isos[i].clone()))
                    .unwrap_or(Action::None),
                KeyCode::Char('r') | KeyCode::Char('R') => Action::RescanIso,
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => Action::Quit,
                _ => Action::None,
            },
            Screen::Devices { devices, state, .. } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    move_sel(state, devices.len(), -1);
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    move_sel(state, devices.len(), 1);
                    Action::None
                }
                KeyCode::Enter => state
                    .selected()
                    .map(|i| Action::AskConfirm(devices[i].clone()))
                    .unwrap_or(Action::None),
                KeyCode::Char('r') | KeyCode::Char('R') => Action::RescanDevices,
                KeyCode::Char('a') | KeyCode::Char('A') => Action::ToggleAll,
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => Action::Quit,
                _ => Action::None,
            },
            Screen::Confirm { device, yes } => match key.code {
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Tab
                | KeyCode::Char('h')
                | KeyCode::Char('l') => {
                    *yes = !*yes;
                    Action::None
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    *yes = true;
                    Action::None
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Action::Back,
                KeyCode::Enter => {
                    if *yes {
                        Action::Start(device.clone())
                    } else {
                        Action::Back
                    }
                }
                _ => Action::None,
            },
            Screen::Installing(_) => Action::None,
            Screen::Done { .. } => match key.code {
                KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                    Action::Quit
                }
                _ => Action::None,
            },
        };

        match action {
            Action::None => {}
            Action::Quit => self.quit = true,
            Action::RescanIso => self.scan_isos(),
            Action::UseIso(p) => self.use_iso(p),
            Action::ToggleAll => {
                self.show_all = !self.show_all;
                self.to_devices(None);
            }
            Action::RescanDevices | Action::Back => self.to_devices(None),
            Action::AskConfirm(d) => self.screen = Screen::Confirm { device: d, yes: false },
            Action::Start(d) => self.start(d),
        }
    }

    // ---- Background messages --------------------------------------------

    pub fn tick(&mut self) {
        let mut finished: Option<Result<(), String>> = None;
        if let Screen::Installing(st) = &mut self.screen {
            loop {
                match st.rx.try_recv() {
                    Ok(Msg::Stage(i)) => {
                        st.stage = i;
                        st.progress = 0.0;
                        st.status.clear();
                    }
                    Ok(Msg::Log(l)) => {
                        st.log.push(l);
                        if st.log.len() > 500 {
                            st.log.drain(..100);
                        }
                    }
                    Ok(Msg::Status(s)) => st.status = s,
                    Ok(Msg::Progress(p)) => st.progress = p,
                    Ok(Msg::Finished(r)) => {
                        finished = Some(r);
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = Some(Err("the worker thread stopped unexpectedly".into()));
                        break;
                    }
                }
            }
        }

        if let Some(res) = finished {
            let log = match &self.screen {
                Screen::Installing(st) => st.log.clone(),
                _ => Vec::new(),
            };
            // Final step of the workflow: unmount the ISO.
            let iso_errs = self.iso_mounts.unmount_all();
            let (ok, mut message) = match res {
                Ok(()) => (
                    true,
                    "Your Windows 11 installer USB is ready. It is safe to unplug the drive."
                        .to_string(),
                ),
                Err(e) => (
                    false,
                    format!("The installer failed: {e}\n\nThe drive is in an incomplete state; re-run to try again."),
                ),
            };
            if !iso_errs.is_empty() {
                message.push_str(&format!("\n\nWarning: could not unmount the ISO: {}", iso_errs.join("; ")));
            }
            self.screen = Screen::Done { ok, message, log };
        }
    }
}
