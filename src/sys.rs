//! Small helpers around external commands and mount bookkeeping.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

/// Make sure the sbin directories are searchable (some sudo configs drop them).
pub fn extend_path() {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for extra in [
        "/usr/local/sbin",
        "/usr/local/bin",
        "/usr/sbin",
        "/usr/bin",
        "/sbin",
        "/bin",
    ] {
        let p = PathBuf::from(extra);
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    }
    if let Ok(joined) = std::env::join_paths(dirs) {
        std::env::set_var("PATH", joined);
    }
}

pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

/// Tools the program shells out to, with the package that usually provides them.
pub fn missing_tools() -> Vec<String> {
    let reqs: &[(&[&str], &str)] = &[
        (&["lsblk"], "util-linux"),
        (&["mount"], "util-linux"),
        (&["umount"], "util-linux"),
        (&["wipefs"], "util-linux"),
        (&["parted"], "parted"),
        (&["mkfs.vfat"], "dosfstools"),
        (&["mkfs.ntfs", "mkntfs"], "ntfs-3g (provides mkfs.ntfs)"),
        (&["rsync"], "rsync"),
        (&["findmnt"], "util-linux"),
        (&["sha256sum"], "coreutils"),
    ];
    reqs.iter()
        .filter(|(names, _)| !names.iter().any(|n| which(n).is_some()))
        .map(|(names, pkg)| format!("{}  (package: {})", names.join(" / "), pkg))
        .collect()
}

/// Run a command, returning stdout, or an error that includes stderr.
pub fn run<I, S>(cmd: &str, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_os_string()).collect();
    let out = Command::new(cmd)
        .args(&args)
        .output()
        .with_context(|| format!("could not start `{cmd}`"))?;
    if !out.status.success() {
        let printable: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        bail!(
            "`{} {}` failed ({}): {}",
            cmd,
            printable.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn unescape(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// (source, mountpoint) pairs from /proc/mounts.
pub fn proc_mounts() -> Vec<(String, PathBuf)> {
    fs::read_to_string("/proc/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let src = it.next()?;
            let tgt = it.next()?;
            Some((unescape(src), PathBuf::from(unescape(tgt))))
        })
        .collect()
}

pub fn is_mounted(path: &Path) -> bool {
    let Ok(canon) = fs::canonicalize(path) else {
        return false;
    };
    proc_mounts().iter().any(|(_, t)| *t == canon)
}

/// Unmount with a few retries (the kernel can briefly report "busy" after heavy I/O).
pub fn umount<P: AsRef<OsStr>>(target: P) -> Result<()> {
    let mut last = None;
    for _ in 0..6 {
        match run("umount", [target.as_ref()]) {
            Ok(_) => return Ok(()),
            Err(e) => {
                last = Some(e);
                sleep(Duration::from_secs(1));
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("umount failed")))
}

/// Total size in bytes of all regular files under `path` (does not follow symlinks).
/// Any unreadable file or directory is an error, so a size is never underestimated.
pub fn dir_size(path: &Path) -> Result<u64> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    if meta.is_file() {
        return Ok(meta.len());
    }
    if !meta.is_dir() {
        return Ok(0);
    }
    let mut total = 0;
    for entry in fs::read_dir(path).with_context(|| format!("could not list {}", path.display()))? {
        let entry = entry.with_context(|| format!("could not list {}", path.display()))?;
        total += dir_size(&entry.path())?;
    }
    Ok(total)
}

/// Case-insensitive lookup of a directory entry (UDF/ISO9660 are case-sensitive on Linux).
pub fn find_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .map_or(false, |n| n.to_string_lossy().eq_ignore_ascii_case(name))
        })
}

/// Tracks mounts we created so they are always cleaned up (also on drop).
#[derive(Default)]
pub struct MountSet {
    mounted: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
}

impl MountSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create `target` (if needed) and mount `source` on it.
    pub fn mount(
        &mut self,
        source: impl AsRef<OsStr>,
        target: &Path,
        fstype: &str,
        opts: &str,
    ) -> Result<()> {
        if is_mounted(target) {
            umount(target).with_context(|| format!("stale mount on {}", target.display()))?;
        }
        fs::create_dir_all(target)
            .with_context(|| format!("could not create {}", target.display()))?;
        if !self.dirs.iter().any(|d| d == target) {
            self.dirs.push(target.to_path_buf());
        }
        run(
            "mount",
            [
                OsStr::new("-t"),
                OsStr::new(fstype),
                OsStr::new("-o"),
                OsStr::new(opts),
                source.as_ref(),
                target.as_os_str(),
            ],
        )?;
        self.mounted.push(target.to_path_buf());
        Ok(())
    }

    /// Unmount everything (newest first) and remove the now-empty directories.
    /// Returns a list of error messages.
    pub fn unmount_all(&mut self) -> Vec<String> {
        let mut errs = Vec::new();
        for p in self.mounted.drain(..).rev() {
            if is_mounted(&p) {
                if let Err(e) = umount(&p) {
                    errs.push(format!("{e:#}"));
                }
            }
        }
        for d in self.dirs.drain(..) {
            if !is_mounted(&d) {
                let _ = fs::remove_dir(&d);
            }
        }
        errs
    }
}

impl Drop for MountSet {
    fn drop(&mut self) {
        let _ = self.unmount_all();
    }
}
