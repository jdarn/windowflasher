# windowflasher

A single-binary TUI that builds a bootable Windows 11 installer USB from a Linux host.
Everything happens in the directory you run it from (`./iso`, `./vfat`, `./ntfs` are
created as temporary mount points and removed afterwards).

## Build

```sh
cargo build --release
# fully static binary, easiest to carry around:
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Use

1. Download the ISO from https://www.microsoft.com/en-us/software-download/windows11
2. Put the ISO and the `windowflasher` binary in the same folder.
3. Run `./windowflasher` (it re-launches itself through `sudo` if it is not root).
4. Pick the USB flash drive with ↑/↓ + Enter, then confirm (the prompt defaults to **No**).

## Required system tools

`lsblk findmnt mount umount wipefs parted mkfs.vfat mkfs.ntfs rsync sha256sum` (packages:
util-linux, parted, dosfstools, ntfs-3g, rsync, coreutils). The binary checks for them at startup and tells you what is missing.
NTFS is mounted with the kernel `ntfs3` driver, falling back to `ntfs-3g`.

## Notes

- Both partitions get the GPT `msftdata` flag. The drive is GPT-only, so it boots in UEFI mode
  (which Windows 11 requires anyway).
- Before the confirm screen, the drive size is checked against the ISO (1 GiB BOOT + ISO contents
  + headroom), and the ISO's boot files are checked to fit on BOOT, so nothing is wiped for a
  drive that is too small.
- After copying, `boot.wim` and `install.wim`/`install.esd` are verified with sha256 against the
  ISO. This takes a few minutes on slow drives.
- Partitions grabbed by a desktop automounter are unmounted again before copying and at the end.
- **Flash-drive detection** is a heuristic: whole disk, `TRAN=usb`, removable (`RM=1`),
  not read-only, size > 0, and no `ssd`/`hdd`/`nvme` in the model string. USB SSDs and HDDs normally
  report as non-removable, which is what separates them. `ROTA` is ignored because many flash
  drives report `ROTA=1`. A flash drive that reports itself as non-removable is hidden by default.
- When nothing is listed, the screen says why for each USB disk. Press **A** to also list USB disks
  that might be SSD/HDD (they are marked with a warning and flagged again on the confirm screen).
- `./windowflasher --list` prints the raw `lsblk` table and the verdict for every disk (no root needed).
- The drive holding the current directory is never listed, and the selected drive is re-checked right before wiping.
- Ctrl-C is ignored while writing, so a half-written drive and stray mounts are not left behind.
