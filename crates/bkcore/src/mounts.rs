//! Reading the Linux mount table.
//!
//! The parsing is pure (text in, entries out) so it is tested on every
//! platform; only the callers in `file::sys` and `opt::media` touch
//! `/proc/self/mounts`.

/// One line of `/proc/self/mounts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// `/dev/sda1`, `tmpfs`, `//nas/share`, ...
    pub source: String,
    pub mount_point: String,
    /// `ext4`, `ntfs3`, `vfat`, `exfat`, `fuseblk`, ...
    pub fstype: String,
    pub options: String,
}

/// Parse the whole table. Malformed lines are skipped.
pub fn parse(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let source = unescape(f.next()?);
            let mount_point = unescape(f.next()?);
            let fstype = f.next()?.to_string();
            let options = f.next().unwrap_or("").to_string();
            Some(Mount {
                source,
                mount_point,
                fstype,
                options,
            })
        })
        .collect()
}

/// Undo the octal escapes the kernel uses for whitespace in paths:
/// `\040` space, `\011` tab, `\012` newline, `\134` backslash.
pub fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = (oct[0] - b'0') * 64 + (oct[1] - b'0') * 8 + (oct[2] - b'0');
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The mount holding `path`: the entry with the longest mount point that is
/// an ancestor of (or equal to) the path, comparing whole components so
/// `/mnt/data` does not claim `/mnt/data2`.
pub fn find<'a>(mounts: &'a [Mount], path: &str) -> Option<&'a Mount> {
    let path = path.trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };
    mounts
        .iter()
        .filter(|m| {
            let mp = m.mount_point.trim_end_matches('/');
            if mp.is_empty() {
                return true; // the root filesystem holds everything
            }
            path == mp || path.starts_with(&format!("{mp}/"))
        })
        .max_by_key(|m| m.mount_point.trim_end_matches('/').len())
}

/// Is this a filesystem that holds user data, as opposed to a kernel-synthesized
/// one (`proc`, `sysfs`, `cgroup2`, ...) or a loop-mounted package image?
pub fn is_real_filesystem(fstype: &str) -> bool {
    matches!(
        fstype,
        "ext2"
            | "ext3"
            | "ext4"
            | "xfs"
            | "btrfs"
            | "f2fs"
            | "zfs"
            | "bcachefs"
            | "jfs"
            | "reiserfs"
            | "nilfs2"
            | "ntfs"
            | "ntfs3"
            | "fuseblk"
            | "vfat"
            | "msdos"
            | "exfat"
            | "hfsplus"
            | "udf"
            | "iso9660"
    )
}

/// The whole-disk device a partition name belongs to, by naming convention:
/// `sda1` -> `sda`, `nvme0n1p2` -> `nvme0n1`, `mmcblk0p1` -> `mmcblk0`.
/// A name that is not a partition comes back unchanged. On Linux the caller
/// prefers sysfs, which is authoritative; this is the fallback.
pub fn disk_of_partition(name: &str) -> String {
    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed.len() == name.len() || trimmed.is_empty() {
        return name.to_string();
    }
    // Whole devices whose names end in digits: `md127`, `loop5`, `nbd0`,
    // `zram0`, `ram1`. Their partitions carry a `p` (`md127p1`), handled below.
    if matches!(
        trimmed,
        "md" | "loop" | "nbd" | "zram" | "ram" | "nvme" | "mmcblk"
    ) {
        return name.to_string();
    }
    // `nvme0n1p2` / `mmcblk0p1`: a `p` separator after a digit.
    if let Some(stem) = trimmed.strip_suffix('p') {
        if stem.ends_with(|c: char| c.is_ascii_digit()) {
            return stem.to_string();
        }
    }
    // `sda1`, `vdb3`, `hda2`: letters then digits.
    if trimmed.chars().all(|c| c.is_ascii_alphabetic()) {
        return trimmed.to_string();
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
/dev/nvme0n1p2 / ext4 rw,relatime 0 0
/dev/nvme0n1p1 /boot/efi vfat rw,relatime,fmask=0022 0 0
/dev/sda1 /mnt/data ext4 rw,relatime 0 0
/dev/sdb1 /mnt/data2 ntfs3 rw,relatime 0 0
/dev/sdc1 /media/deck/My\\040Games exfat rw,nosuid 0 0
tmpfs /run tmpfs rw,nosuid,nodev 0 0
/dev/loop5 /snap/core/1234 squashfs ro,nodev 0 0
";

    #[test]
    fn the_table_parses_and_unescapes() {
        let m = parse(SAMPLE);
        assert_eq!(m.len(), 8);
        assert_eq!(m[1].source, "/dev/nvme0n1p2");
        assert_eq!(m[1].mount_point, "/");
        assert_eq!(m[1].fstype, "ext4");
        assert_eq!(m[5].mount_point, "/media/deck/My Games");
        assert_eq!(unescape("a\\134b\\011c"), "a\\b\tc");
        assert_eq!(unescape("no\\escapes\\here"), "no\\escapes\\here");
    }

    #[test]
    fn the_longest_mount_point_wins_by_whole_components() {
        let m = parse(SAMPLE);
        assert_eq!(find(&m, "/home/deck").unwrap().mount_point, "/");
        assert_eq!(find(&m, "/mnt/data/photos").unwrap().source, "/dev/sda1");
        assert_eq!(find(&m, "/mnt/data2/x").unwrap().source, "/dev/sdb1");
        assert_eq!(find(&m, "/mnt/data2").unwrap().source, "/dev/sdb1");
        assert_eq!(find(&m, "/boot/efi/EFI").unwrap().fstype, "vfat");
        assert_eq!(find(&m, "/").unwrap().mount_point, "/");
        assert_eq!(find(&m, "/media/deck/My Games/x").unwrap().fstype, "exfat");
    }

    #[test]
    fn real_filesystems_are_told_from_pseudo_ones() {
        assert!(is_real_filesystem("ext4"));
        assert!(is_real_filesystem("ntfs3"));
        assert!(is_real_filesystem("exfat"));
        assert!(!is_real_filesystem("proc"));
        assert!(!is_real_filesystem("tmpfs"));
        assert!(!is_real_filesystem("squashfs"));
        assert!(!is_real_filesystem("cgroup2"));
    }

    #[test]
    fn partition_names_map_to_their_disk() {
        assert_eq!(disk_of_partition("sda1"), "sda");
        assert_eq!(disk_of_partition("sdab12"), "sdab");
        assert_eq!(disk_of_partition("nvme0n1p2"), "nvme0n1");
        assert_eq!(disk_of_partition("mmcblk0p1"), "mmcblk0");
        assert_eq!(disk_of_partition("sda"), "sda");
        assert_eq!(disk_of_partition("nvme0n1"), "nvme0n1");
        assert_eq!(disk_of_partition("dm-0"), "dm-0");
        assert_eq!(disk_of_partition("md127"), "md127");
    }
}
