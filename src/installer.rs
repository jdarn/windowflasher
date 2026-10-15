//! The worker that partitions, formats and copies. Runs on its own thread and
//! reports back over a channel.

use crate::devices::{human_size, partition_path, Device};
use crate::sys::{self, MountSet};
use anyhow::{anyhow, bail, Context, Result};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

pub const STAGES: [&str; 7] = [
    "Partition drive (GPT)",
    "Format BOOT (FAT32) and INSTALL (NTFS)",
    "Copy boot files to BOOT",
    "Copy boot.wim to BOOT",
    "Copy Windows files to INSTALL",
    "Flush and verify (sha256)",
    "Unmount",
];

/// Size of the FAT32 BOOT partition, in MiB. INSTALL starts right after it.
pub const BOOT_MIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

/// Bytes the drive needs: 1 MiB alignment gap, BOOT, the ISO contents on NTFS,
/// plus headroom for NTFS metadata and the GPT backup header.
pub fn required_drive_bytes(iso_bytes: u64) -> u64 {
    (1 + BOOT_MIB) * MIB + iso_bytes + iso_bytes / 50 + 64 * MIB
}

/// Usable space on BOOT for files, leaving room for FAT32 tables and cluster slack.
pub fn boot_capacity_bytes() -> u64 {
    (BOOT_MIB - 24) * MIB
}

pub enum Msg {
    Stage(usize),
    Log(String),
    Status(String),
    Progress(f64),
    Finished(Result<(), String>),
}

pub struct Job {
    pub device: Device,
    pub iso_dir: PathBuf,
    pub work_dir: PathBuf,
}

pub fn spawn(job: Job, tx: Sender<Msg>) {
    thread::spawn(move || {
        let res = run_job(&job, &tx).map_err(|e| format!("{e:#}"));
        let _ = tx.send(Msg::Finished(res));
    });
}

fn log(tx: &Sender<Msg>, s: impl Into<String>) {
    let _ = tx.send(Msg::Log(s.into()));
}

fn stage(tx: &Sender<Msg>, i: usize) {
    let _ = tx.send(Msg::Stage(i));
    log(tx, format!("== {} ==", STAGES[i]));
}

fn run_job(job: &Job, tx: &Sender<Msg>) -> Result<()> {
    let mut mounts = MountSet::new();
    let result = steps(job, tx, &mut mounts);
    // Always try to leave nothing mounted, even after a failure.
    let errs = mounts.unmount_all();
    result?;
    if !errs.is_empty() {
        bail!("unmount problems: {}", errs.join("; "));
    }
    Ok(())
}

fn steps(job: &Job, tx: &Sender<Msg>, mounts: &mut MountSet) -> Result<()> {
    let dev = job.device.path.as_str();
    let p1 = partition_path(dev, 1);
    let p2 = partition_path(dev, 2);
    let vfat = job.work_dir.join("vfat");
    let ntfs = job.work_dir.join("ntfs");
    let iso = format!("{}/", job.iso_dir.display());

    // 1. Partition
    stage(tx, 0);
    unmount_device(dev, tx)?;
    log(tx, format!("Wiping old signatures on {dev}"));
    sys::run("wipefs", ["-a", dev])?;
    let boot_end = format!("{}MiB", 1 + BOOT_MIB);
    log(
        tx,
        format!("Creating GPT with BOOT ({BOOT_MIB} MiB, FAT32) and INSTALL (rest, NTFS)"),
    );
    sys::run(
        "parted",
        [
            "-s", "-a", "optimal", dev, "mklabel", "gpt", "mkpart", "BOOT", "fat32", "1MiB",
            boot_end.as_str(), "mkpart", "INSTALL", "ntfs", boot_end.as_str(), "100%", "set", "1",
            "msftdata", "on", "set", "2", "msftdata", "on",
        ],
    )?;
    settle(dev, &[p1.as_str(), p2.as_str()])?;

    // 2. Format
    stage(tx, 1);
    // A desktop automounter may have grabbed the new partitions.
    unmount_device(dev, tx)?;
    sys::run("mkfs.vfat", ["-F", "32", "-n", "BOOT", p1.as_str()])?;
    log(tx, format!("Formatted {p1} as FAT32 (BOOT)"));
    let mkntfs = if sys::which("mkfs.ntfs").is_some() { "mkfs.ntfs" } else { "mkntfs" };
    sys::run(mkntfs, ["-Q", "-L", "INSTALL", p2.as_str()])?;
    log(tx, format!("Formatted {p2} as NTFS (INSTALL)"));

    // 3. Copy everything except sources/ to the FAT32 partition
    stage(tx, 2);
    // The automounter may have grabbed the freshly formatted partitions.
    let _ = sys::run("udevadm", ["settle"]);
    unmount_device(dev, tx)?;
    mounts.mount(&p1, &vfat, "vfat", "rw")?;
    run_rsync(
        tx,
        &[
            "-r".into(),
            "--info=progress2".into(),
            "--no-inc-recursive".into(),
            "--exclude".into(),
            "/sources".into(),
            "--delete-before".into(),
            iso.clone(),
            format!("{}/", vfat.display()),
        ],
    )?;

    // 4. boot.wim only
    stage(tx, 3);
    let src_sources = sys::find_ci(&job.iso_dir, "sources")
        .ok_or_else(|| anyhow!("ISO has no sources directory"))?;
    let boot_wim = sys::find_ci(&src_sources, "boot.wim")
        .ok_or_else(|| anyhow!("ISO has no sources/boot.wim"))?;
    let dst_sources = vfat.join("sources");
    fs::create_dir_all(&dst_sources)?;
    copy_with_progress(&boot_wim, &dst_sources.join("boot.wim"), tx)?;

    // 5. Whole ISO to NTFS
    stage(tx, 4);
    mount_ntfs(mounts, &p2, &ntfs, tx)?;
    run_rsync(
        tx,
        &[
            "-r".into(),
            "--info=progress2".into(),
            "--no-inc-recursive".into(),
            "--delete-before".into(),
            iso,
            format!("{}/", ntfs.display()),
        ],
    )?;

    // 6. Sync + verify what actually landed on the drive
    stage(tx, 5);
    let _ = tx.send(Msg::Status("Flushing data to the drive; this can take a while...".into()));
    unsafe { libc::sync() };
    log(tx, "Filesystems synced");
    // Drop the page cache so the hashes read from the USB drive, not from RAM.
    let _ = fs::write("/proc/sys/vm/drop_caches", "3");
    verify_copy(tx, &job.iso_dir, &vfat, &ntfs)?;

    // 7. Unmount ours, then anything a desktop automounter added meanwhile
    stage(tx, 6);
    let errs = mounts.unmount_all();
    if !errs.is_empty() {
        bail!("could not unmount cleanly: {}", errs.join("; "));
    }
    unmount_device(dev, tx)?;
    unsafe { libc::sync() };
    log(tx, "BOOT and INSTALL unmounted");
    let _ = tx.send(Msg::Progress(1.0));
    Ok(())
}

fn unmount_device(dev: &str, tx: &Sender<Msg>) -> Result<()> {
    for (src, tgt) in sys::proc_mounts() {
        if let Some(rest) = src.strip_prefix(dev) {
            if rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit() || c == 'p') {
                log(tx, format!("Unmounting {} ({})", src, tgt.display()));
                sys::umount(&tgt)
                    .with_context(|| format!("could not unmount {} (is it in use?)", tgt.display()))?;
            }
        }
    }
    Ok(())
}

fn settle(dev: &str, parts: &[&str]) -> Result<()> {
    let _ = sys::run("udevadm", ["settle"]);
    let _ = sys::run("partprobe", [dev]);
    let _ = sys::run("udevadm", ["settle"]);
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if parts.iter().all(|p| Path::new(p).exists()) {
            sleep(Duration::from_millis(500));
            return Ok(());
        }
        sleep(Duration::from_millis(300));
    }
    bail!("partition devices did not appear: {}", parts.join(", "))
}

fn mount_ntfs(mounts: &mut MountSet, part: &str, target: &Path, tx: &Sender<Msg>) -> Result<()> {
    match mounts.mount(part, target, "ntfs3", "rw") {
        Ok(()) => Ok(()),
        Err(e1) => {
            log(tx, "Kernel ntfs3 driver unavailable, trying ntfs-3g");
            mounts
                .mount(part, target, "ntfs-3g", "rw")
                .map_err(|e2| anyhow!("could not mount NTFS. ntfs3: {e1:#}; ntfs-3g: {e2:#}"))
        }
    }
}

fn parse_percent(line: &str) -> Option<f64> {
    line.split_whitespace()
        .find(|t| t.ends_with('%'))
        .and_then(|t| t.trim_end_matches('%').parse::<f64>().ok())
}

fn handle_line(tx: &Sender<Msg>, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    match parse_percent(line) {
        Some(p) => {
            let _ = tx.send(Msg::Progress((p / 100.0).clamp(0.0, 1.0)));
            let _ = tx.send(Msg::Status(line.to_string()));
        }
        None => log(tx, line),
    }
}

/// rsync prints progress with carriage returns, so split on both \r and \n.
fn run_rsync(tx: &Sender<Msg>, args: &[String]) -> Result<()> {
    let mut child = Command::new("rsync")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start rsync")?;
    let mut stderr = child.stderr.take().ok_or_else(|| anyhow!("no rsync stderr"))?;
    let err_thread = thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let mut stdout = child.stdout.take().ok_or_else(|| anyhow!("no rsync stdout"))?;
    let mut buf = [0u8; 4096];
    let mut line: Vec<u8> = Vec::new();
    loop {
        let n = stdout.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            if b == b'\r' || b == b'\n' {
                if !line.is_empty() {
                    handle_line(tx, &String::from_utf8_lossy(&line));
                    line.clear();
                }
            } else {
                line.push(b);
            }
        }
    }
    let status = child.wait()?;
    let err = err_thread.join().unwrap_or_default();
    if !status.success() {
        bail!("rsync failed ({status}): {}", err.trim());
    }
    Ok(())
}

fn copy_with_progress(src: &Path, dst: &Path, tx: &Sender<Msg>) -> Result<()> {
    let mut input = File::open(src).with_context(|| format!("open {}", src.display()))?;
    let total = input.metadata()?.len().max(1);
    let mut out = File::create(dst).with_context(|| format!("create {}", dst.display()))?;
    let mut buf = vec![0u8; 4 << 20];
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        if last.elapsed() > Duration::from_millis(200) {
            let _ = tx.send(Msg::Progress(done as f64 / total as f64));
            let _ = tx.send(Msg::Status(format!(
                "boot.wim: {} / {}",
                human_size(done),
                human_size(total)
            )));
            last = Instant::now();
        }
    }
    out.sync_all()?;
    let _ = tx.send(Msg::Progress(1.0));
    log(tx, format!("Copied boot.wim ({})", human_size(total)));
    Ok(())
}

fn sha256(path: &Path) -> Result<String> {
    let out = sys::run("sha256sum", [path])?;
    out.split_whitespace()
    .next()
    .map(str::to_owned)
    .ok_or_else(|| anyhow!("no sha256sum output for {}", path.display()))
}

/// Compare boot.wim and install.wim/esd on the drive against the ISO.
fn verify_copy(tx: &Sender<Msg>, iso: &Path, vfat: &Path, ntfs: &Path) -> Result<()> {
    let src_sources = sys::find_ci(iso, "sources").ok_or_else(|| anyhow!("ISO has no sources directory"))?;
    let boot_src = sys::find_ci(&src_sources, "boot.wim").ok_or_else(|| anyhow!("ISO has no boot.wim"))?;
    let install_src = sys::find_ci(&src_sources, "install.wim")
    .or_else(|| sys::find_ci(&src_sources, "install.esd"))
    .ok_or_else(|| anyhow!("ISO has no install.wim/esd"))?;
    let ntfs_sources = sys::find_ci(ntfs, "sources").ok_or_else(|| anyhow!("INSTALL has no sources directory"))?;

    // rsync preserves names, so the NTFS copies keep the ISO's file names.
    let boot_name = boot_src.file_name().unwrap_or_default().to_os_string();
    let install_name = install_src.file_name().unwrap_or_default().to_os_string();

    let checks = vec![
        ("BOOT", boot_src.clone(), vfat.join("sources").join("boot.wim")),
        ("INSTALL", boot_src.clone(), ntfs_sources.join(&boot_name)),
        ("INSTALL", install_src.clone(), ntfs_sources.join(&install_name)),
    ];

    for (i, (part, src, dst)) in checks.iter().enumerate() {
        let name = src.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let _ = tx.send(Msg::Progress(i as f64 / checks.len() as f64));
        let _ = tx.send(Msg::Status(format!("Verifying {name} on {part}...")));
        let (want, got) = (sha256(src)?, sha256(dst)?);
        if want != got {
            bail!("checksum mismatch for {name} on {part}: ISO {want}, drive {got}");
        }
        log(tx, format!("OK  {name} on {part}  sha256 {}", want.get(..16).unwrap_or(&want)));
    }
    let _ = tx.send(Msg::Progress(1.0));
    Ok(())
}
