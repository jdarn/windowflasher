//! USB flash drive discovery via lsblk.

use crate::sys;
use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Device {
    pub path: String,
    pub label: String,
    pub size: u64,
    /// Set when the drive is only listed because "show all USB disks" is on.
    pub warn: Option<String>,
}

pub enum Verdict {
    /// Looks like a USB flash drive: listed by default.
    Flash,
    /// A USB disk that might be an SSD/HDD (or a flash drive that reports
    /// itself oddly). Hidden unless "show all USB disks" is on.
    Maybe(String),
    /// Never offered.
    Reject(String),
}

pub struct Entry {
    pub device: Device,
    pub tran: String,
    pub verdict: Verdict,
}

pub struct Scan {
    pub devices: Vec<Device>,
    /// Human-readable reasons for disks that were hidden or ignored.
    pub why: Vec<String>,
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_u64().map_or(false, |x| x != 0),
        Some(Value::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn size_of(v: &Value) -> u64 {
    match v.get("size") {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// Whole disk that backs the current directory, so we never offer to wipe
/// the drive the binary and ISO are sitting on.
fn disk_backing_cwd() -> Option<String> {
    let src = sys::run("findmnt", ["-n", "-o", "SOURCE", "--target", "."]).ok()?;
    let src = src.trim().split('[').next()?.trim().to_string();
    if !src.starts_with("/dev/") {
        return None;
    }
    let pk = sys::run("lsblk", ["-no", "PKNAME", src.as_str()]).ok()?;
    let pk = pk.lines().next().unwrap_or("").trim().to_string();
    Some(if pk.is_empty() { src } else { format!("/dev/{pk}") })
}

/// Classify one lsblk entry. Returns None for things that are not whole disks
/// (loop devices, etc).
///
/// Rules for a USB disk to count as a flash drive:
///   * transport is USB, not read-only, has media (size > 0)
///   * flagged removable (RM=1): flash drives set this, external SSD/HDD
///     enclosures normally do not
///   * model/vendor does not say SSD/HDD/NVMe
/// ROTA is deliberately ignored: many USB flash drives report ROTA=1.
fn classify(d: &Value, skip: Option<&str>) -> Option<Entry> {
    if text(d, "type") != "disk" {
        return None;
    }
    let name = text(d, "name");
    let path = {
        let p = text(d, "path");
        if p.is_empty() {
            format!("/dev/{name}")
        } else {
            p
        }
    };
    let tran = text(d, "tran");
    let size = size_of(d);
    let label = format!("{} {}", text(d, "vendor"), text(d, "model"))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let label = if label.is_empty() { "Unnamed drive".to_string() } else { label };
    let lower = label.to_lowercase();
    let named_fast = ["ssd", "hdd", "nvme"].iter().any(|w| lower.contains(w));
    let removable = truthy(d.get("rm"));

    let verdict = if tran != "usb" {
        Verdict::Reject(format!(
            "not on the USB bus (transport: {})",
            if tran.is_empty() { "none" } else { tran.as_str() }
        ))
    } else if skip == Some(path.as_str()) {
        Verdict::Reject("the current directory is on this drive".to_string())
    } else if truthy(d.get("ro")) {
        Verdict::Reject("read-only".to_string())
    } else if size == 0 {
        Verdict::Reject("no media (size 0)".to_string())
    } else if !removable && named_fast {
        Verdict::Maybe("not flagged removable and the model name says SSD/HDD".to_string())
    } else if !removable {
        Verdict::Maybe(
            "not flagged removable (RM=0): a flash drive that reports itself as fixed, or an external SSD/HDD"
                .to_string(),
        )
    } else if named_fast {
        Verdict::Maybe("the model name says SSD/HDD".to_string())
    } else {
        Verdict::Flash
    };

    Some(Entry {
        device: Device { path, label, size, warn: None },
        tran,
        verdict,
    })
}

pub fn classify_all() -> Result<Vec<Entry>> {
    let out = sys::run(
        "lsblk",
        ["-J", "-b", "-d", "-o", "NAME,PATH,TYPE,TRAN,RM,RO,ROTA,SIZE,MODEL,VENDOR"],
    )
    .context("lsblk failed")?;
    let json: Value = serde_json::from_str(&out).context("could not parse lsblk output")?;
    let skip = disk_backing_cwd();
    let mut v: Vec<Entry> = json["blockdevices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| classify(d, skip.as_deref()))
        .collect();
    v.sort_by(|a, b| a.device.path.cmp(&b.device.path));
    Ok(v)
}

/// List candidate drives. With `show_all`, USB disks that might be SSD/HDD
/// are included too (each carries a warning); otherwise they are reported in `why`.
pub fn scan(show_all: bool) -> Result<Scan> {
    let mut devices = Vec::new();
    let mut why = Vec::new();
    let mut elsewhere = Vec::new();
    for e in classify_all()? {
        let desc = format!(
            "{} ({}, {})",
            e.device.path,
            e.device.label,
            human_size(e.device.size)
        );
        match e.verdict {
            Verdict::Flash => devices.push(e.device),
            Verdict::Maybe(w) => {
                if show_all {
                    let mut dev = e.device;
                    dev.warn = Some(w);
                    devices.push(dev);
                } else {
                    why.push(format!("{desc}: hidden, {w}"));
                }
            }
            Verdict::Reject(r) => {
                if e.tran == "usb" {
                    why.push(format!("{desc}: {r}"));
                } else {
                    elsewhere.push(format!(
                        "{} [{}]",
                        e.device.path,
                        if e.tran.is_empty() { "none" } else { e.tran.as_str() }
                    ));
                }
            }
        }
    }
    if !elsewhere.is_empty() {
        why.push(format!("Not on the USB bus (ignored): {}", elsewhere.join(", ")));
    }
    Ok(Scan { devices, why })
}

/// `windowflasher --list`: print what lsblk sees and what we decided about each disk.
/// Handy for debugging; needs no root.
pub fn print_report() -> Result<()> {
    println!("lsblk view of whole disks:");
    print!(
        "{}",
        sys::run(
            "lsblk",
            ["-d", "-o", "NAME,PATH,TYPE,TRAN,RM,RO,ROTA,SIZE,MODEL,VENDOR"]
        )?
    );
    println!("\nwindowflasher verdicts:");
    for e in classify_all()? {
        let v = match &e.verdict {
            Verdict::Flash => "LISTED (flash drive)".to_string(),
            Verdict::Maybe(w) => format!("hidden by default: {w}"),
            Verdict::Reject(r) => format!("ignored: {r}"),
        };
        println!("  {:<14} {:<28} {}", e.device.path, e.device.label, v);
    }
    Ok(())
}

/// /dev/sdb -> /dev/sdb1, /dev/mmcblk0 -> /dev/mmcblk0p1
pub fn partition_path(dev: &str, n: u32) -> String {
    if dev.chars().last().map_or(false, |c| c.is_ascii_digit()) {
        format!("{dev}p{n}")
    } else {
        format!("{dev}{n}")
    }
}

pub fn human_size(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1000.0 && i < 4 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}
