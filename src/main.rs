mod app;
mod devices;
mod installer;
mod sys;
mod ui;

use anyhow::{bail, Result};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

fn main() -> Result<()> {
    sys::extend_path();

    // `windowflasher --list`: show what lsblk sees and why each disk is or isn't offered.
    if std::env::args().nth(1).as_deref() == Some("--list") {
        return devices::print_report();
    }

    // Partitioning and mounting need root: re-run ourselves through sudo.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("windowflasher needs root to partition and mount; re-running with sudo...");
        let exe = std::env::current_exe()?;
        let err = Command::new("sudo")
            .arg(exe)
            .args(std::env::args_os().skip(1))
            .exec();
        bail!("could not run sudo: {err}. Try: sudo ./windowflasher");
    }

    let missing = sys::missing_tools();
    if !missing.is_empty() {
        eprintln!("Missing required tools:");
        for m in &missing {
            eprintln!("  - {m}");
        }
        eprintln!(
            "\nInstall them, e.g.\n  Debian/Ubuntu: sudo apt install util-linux parted dosfstools ntfs-3g rsync\n  Fedora:        sudo dnf install util-linux parted dosfstools ntfs-3g ntfsprogs rsync\n  Arch:          sudo pacman -S util-linux parted dosfstools ntfs-3g rsync"
        );
        std::process::exit(1);
    }

    let mut app = app::App::new()?;
    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app);
    ratatui::restore();
    drop(app); // unmounts the ISO and removes ./iso if still present
    res
}

fn run(terminal: &mut DefaultTerminal, app: &mut app::App) -> Result<()> {
    while !app.quit {
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    app.on_key(k);
                }
            }
        }
        app.tick();
    }
    Ok(())
}
