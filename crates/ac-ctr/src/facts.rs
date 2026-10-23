//! The host side of `ac-ctl doctor` and `tune` (ADR-014): [`collect`] reads [`HostFacts`] from
//! `/proc`, `/sys`, the mount tables, Kata's configuration, `gateway.env`, the guest initrd and
//! the firewall, through [`HostRead`], which tests fake. It never opens the registry, so it works
//! while `ac-gateway` runs. What only root may read is skipped, and says so, for anyone else.

use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use ac_domain::tune::facts::{self, Disk, Fact, Firewall, HostFacts, Kvm, Mode, NFT_CHAINS};
use platform::process::{Command, Spawner, Stdio};

use crate::read_all;

/// What [`collect`] needs of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Permission bits.
    pub perm: u32,
    /// The device the file is on.
    pub dev: u64,
    /// The device a device node is.
    pub rdev: u64,
}

/// How [`collect`] reads the host.
pub trait HostRead {
    /// A text file.
    fn read(&self, path: &Path) -> io::Result<String>;
    /// A file's owner, mode and devices (following symlinks).
    fn stat(&self, path: &Path) -> io::Result<Stat>;
    /// Open read-write and close again (`/dev/kvm`).
    fn open_rw(&self, path: &Path) -> io::Result<()>;
    /// (total, free) MiB of the filesystem holding `path`.
    fn statfs_mib(&self, path: &Path) -> io::Result<(u64, u64)>;
    /// This process's effective uid.
    fn euid(&self) -> u32;
    /// Run a command; its stdout when it succeeds, else why not.
    fn run(&self, argv: &[&str]) -> Result<String, String>;
    /// The contents of `name` in a (compressed) cpio archive, read as a stream: nothing is
    /// unpacked to disk. `None` when the archive has no such entry.
    fn archive_entry(&self, archive: &Path, name: &str) -> Result<Option<Vec<u8>>, String>;
    /// Every process's command line.
    fn cmdlines(&self) -> Vec<Vec<String>>;
}

/// Where [`collect`] looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `/proc`.
    pub proc: PathBuf,
    /// `/sys`.
    pub sys: PathBuf,
    /// `/dev/kvm`.
    pub kvm: PathBuf,
    /// `/dev/shm`.
    pub shm: PathBuf,
    /// `/etc/fstab`.
    pub fstab: PathBuf,
    /// Kata's `configuration.toml`.
    pub kata: PathBuf,
    /// The gateway's settings.
    pub gateway_env: PathBuf,
    /// The data directory, whose disk is classed.
    pub data: PathBuf,
    /// The API tokens file.
    pub tokens: PathBuf,
    /// The egress allowlist.
    pub allowlist: PathBuf,
}

impl Paths {
    /// The installed layout (docs/install.md), with Kata's configuration at `kata`.
    pub fn system(kata: PathBuf) -> Self {
        Self {
            proc: "/proc".into(),
            sys: "/sys".into(),
            kvm: "/dev/kvm".into(),
            shm: "/dev/shm".into(),
            fstab: "/etc/fstab".into(),
            kata,
            gateway_env: "/etc/agent-cloud/gateway.env".into(),
            data: "/var/lib/agent-cloud".into(),
            tokens: "/etc/agent-cloud/tokens".into(),
            allowlist: "/etc/agent-cloud/egress-allow.txt".into(),
        }
    }

    /// The configuration files `tune` edits (fstab, Kata, gateway.env) looked for under `root`
    /// instead, for trying edits on copies; everything else stays the live host's.
    pub fn rooted(self, root: &Path) -> Self {
        let under = |p: &Path| root.join(p.strip_prefix("/").unwrap_or(p));
        Self {
            fstab: under(&self.fstab),
            kata: under(&self.kata),
            gateway_env: under(&self.gateway_env),
            ..self
        }
    }
}

/// How much [`collect`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The settings and what they rest on: cheap, for the gateway's startup check and `tune`.
    Settings,
    /// Also the prerequisites: `/dev/kvm`, the initrd, the firewall, file modes.
    Full,
}

/// The `fix-kata-initrd.sh` drop-in that loads the NIC modules in the guest.
pub const INITRD_DROPIN: &str = "usr/lib/systemd/system/kata-agent.service.d/10-net-modules.conf";
/// What the drop-in must do.
pub const INITRD_LOADS: &str = "insmod /usr/lib/kata-net/virtio_net.ko";

/// A read's outcome as a fact: a missing file is absent, a root-only one skipped.
fn from_io<T>(path: &Path, r: io::Result<T>) -> Fact<T> {
    match r {
        Ok(v) => Fact::Known(v),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Fact::Absent,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            Fact::Skipped(format!("{} is root-only: run with sudo", path.display()))
        }
        Err(e) => Fact::Invalid(format!("{}: {e}", path.display())),
    }
}

/// Parse a file's text into a fact.
fn parsed<T>(fact: Fact<String>, parse: impl FnOnce(&str) -> Result<Option<T>, String>) -> Fact<T> {
    match fact {
        Fact::Known(text) => match parse(&text) {
            Ok(Some(v)) => Fact::Known(v),
            Ok(None) => Fact::Absent,
            Err(e) => Fact::Invalid(e),
        },
        Fact::Absent => Fact::Absent,
        Fact::Invalid(e) => Fact::Invalid(e),
        Fact::Skipped(e) => Fact::Skipped(e),
    }
}

/// Read this host. `Err` only when its size (`MemTotal`, online CPUs) cannot be read.
pub fn collect(h: &dyn HostRead, p: &Paths, scope: Scope) -> Result<HostFacts, String> {
    let read = |path: &Path| from_io(path, h.read(path));
    let meminfo = p.proc.join("meminfo");
    let mem = h
        .read(&meminfo)
        .ok()
        .and_then(|t| facts::mem_total_mib(&t))
        .ok_or_else(|| format!("no MemTotal in {}", meminfo.display()))?;
    let online = p.sys.join("devices/system/cpu/online");
    let cores = h
        .read(&online)
        .ok()
        .and_then(|t| facts::cpu_count(&t))
        .ok_or_else(|| format!("cannot read {}", online.display()))?;
    let cpuinfo = h.read(&p.proc.join("cpuinfo")).unwrap_or_default();
    let mut f = HostFacts::bare(mem, cores, "not collected");
    f.phys_bits = facts::phys_bits(&cpuinfo).map_or_else(
        || Fact::Skipped("no address sizes in /proc/cpuinfo".into()),
        Fact::Known,
    );
    f.virt = facts::virt(&cpuinfo).map_or(Fact::Absent, Fact::Known);
    f.disk = disk(h, p);
    f.shm = parsed(read(&p.proc.join("self/mounts")), |t| {
        facts::shm_entry(t, mem)
    });
    f.shm_used_mib = from_io(&p.shm, h.statfs_mib(&p.shm)).map(|(t, free)| t.saturating_sub(free));
    f.shm_fstab = parsed(read(&p.fstab), |t| facts::shm_entry(t, mem));
    f.kata = parsed(read(&p.kata), |t| facts::kata(t).map(Some));
    f.gateway = parsed(read(&p.gateway_env), |t| Ok(Some(facts::env_file(t))));
    if scope == Scope::Settings {
        return Ok(f);
    }
    f.kvm = match h.stat(&p.kvm) {
        Err(e) => from_io(&p.kvm, Err(e)),
        Ok(_) => match h.open_rw(&p.kvm) {
            Ok(()) => Fact::Known(Kvm::Usable),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Fact::Known(Kvm::NoAccess),
            Err(e) => Fact::Invalid(e.to_string()),
        },
    };
    f.initrd_net = initrd_net(h, &f.kata);
    f.firewall = firewall(h);
    let mode = |path: &Path| {
        from_io(path, h.stat(path)).map(|s| Mode {
            uid: s.uid,
            gid: s.gid,
            perm: s.perm,
        })
    };
    f.tokens_mode = mode(&p.tokens);
    f.allowlist_mode = mode(&p.allowlist);
    Ok(f)
}

trait MapFact<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Fact<U>;
}

impl<T> MapFact<T> for Fact<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Fact<U> {
        match self {
            Fact::Known(v) => Fact::Known(f(v)),
            Fact::Absent => Fact::Absent,
            Fact::Invalid(e) => Fact::Invalid(e),
            Fact::Skipped(e) => Fact::Skipped(e),
        }
    }
}

/// Linux's `dev_t` as (major, minor).
pub fn major_minor(dev: u64) -> (u64, u64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major, minor)
}

/// The mount source of the filesystem that holds `path`, from `/proc/self/mountinfo`
/// (`42 1 0:34 /root / rw shared:1 - btrfs /dev/sda3 rw`): the mount with the longest mount
/// point containing `path`. Btrfs and overlayfs report an anonymous device (major 0) in `st_dev`,
/// and a btrfs subvolume's is not even the one mountinfo lists; the mount's source names the
/// block device.
pub fn mount_source<'a>(mountinfo: &'a str, path: &Path) -> Option<&'a str> {
    mountinfo
        .lines()
        .filter_map(|l| {
            let point = l.split_whitespace().nth(4)?;
            let (_, after) = l.split_once(" - ")?;
            let point = Path::new(point);
            path.starts_with(point)
                .then(|| Some((point.components().count(), after.split_whitespace().nth(1)?)))?
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, source)| source)
}

/// Rotational or not, for the disk under the data directory: its `st_dev`, or for btrfs and the
/// like the device its mount names (the data directory's path as given: no symlinks resolved); then `queue/rotational` of that device or, for a partition,
/// of its disk.
fn disk(h: &dyn HostRead, p: &Paths) -> Fact<Disk> {
    let Ok(st) = h.stat(&p.data) else {
        return Fact::Skipped(format!("{} not found", p.data.display()));
    };
    let (mut major, mut minor) = major_minor(st.dev);
    if major == 0 {
        let mountinfo = h.read(&p.proc.join("self/mountinfo")).unwrap_or_default();
        let Some(source) = mount_source(&mountinfo, &p.data) else {
            return Fact::Skipped(format!("no mount holds {}", p.data.display()));
        };
        match h.stat(Path::new(source)) {
            Ok(dev) if dev.rdev != 0 => (major, minor) = major_minor(dev.rdev),
            _ => return Fact::Skipped(format!("{source} is not a block device")),
        }
    }
    let dir = p.sys.join(format!("dev/block/{major}:{minor}"));
    let flag = h
        .read(&dir.join("queue/rotational"))
        .or_else(|_| h.read(&dir.join("../queue/rotational")));
    match flag.as_deref().map(str::trim) {
        Ok("1") => Fact::Known(Disk::Rotational),
        Ok("0") => Fact::Known(Disk::Solid),
        _ => Fact::Skipped(format!("no queue/rotational for {major}:{minor}")),
    }
}

fn initrd_net(h: &dyn HostRead, kata: &Fact<facts::Kata>) -> Fact<bool> {
    let Some(kata) = kata.known() else {
        return Fact::Skipped("no Kata configuration".into());
    };
    let Some(initrd) = &kata.initrd else {
        return Fact::Skipped("Kata boots an image, not an initrd".into());
    };
    let path = Path::new(initrd);
    if let Err(e) = h.stat(path) {
        return from_io(path, Err(e));
    }
    match h.archive_entry(path, INITRD_DROPIN) {
        Ok(Some(body)) => Fact::Known(String::from_utf8_lossy(&body).contains(INITRD_LOADS)),
        Ok(None) => Fact::Known(false),
        Err(e) => Fact::Skipped(format!("{initrd}: {e}")),
    }
}

/// Rules in an `nft -a list chain` listing: the lines with a handle, minus the table and chain
/// lines (as `scripts/post-reboot-check.sh` counts them).
pub fn nft_rules(listing: &str) -> u32 {
    let n = listing
        .lines()
        .filter(|l| l.contains("# handle"))
        .filter(|l| !l.trim_start().starts_with("table ") && !l.trim_start().starts_with("chain "))
        .count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn firewall(h: &dyn HostRead) -> Fact<Firewall> {
    if h.euid() != 0 {
        return Fact::Skipped("nft and firewall-cmd need root: run with sudo".into());
    }
    let chains = NFT_CHAINS
        .iter()
        .map(|(chain, _)| {
            let mut argv = vec!["nft", "-a", "list", "chain"];
            argv.extend(chain.split(' '));
            h.run(&argv).ok().map(|out| nft_rules(&out))
        })
        .collect();
    let zone = ["--list-interfaces", "--list-ports", "--list-services"]
        .map(|what| {
            let out = h.run(&["firewall-cmd", "--permanent", "--zone=agents", what]);
            out.unwrap_or_else(|e| e).trim().to_owned()
        })
        .join("|");
    Fact::Known(Firewall { chains, zone })
}

/// Ids of the Kata sandboxes running now: `-id <id>` of each `containerd-shim-kata-v2`, sorted.
/// Every Kata VM picks up a configuration change only when it boots again.
pub fn kata_sandboxes(cmdlines: &[Vec<String>]) -> Vec<String> {
    let mut ids: Vec<String> = cmdlines
        .iter()
        .filter(|argv| {
            argv.first()
                .is_some_and(|a| a.rsplit('/').next() == Some("containerd-shim-kata-v2"))
        })
        .filter_map(|argv| {
            let at = argv.iter().position(|a| a == "-id")?;
            argv.get(at + 1).cloned()
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// `name`'s contents in a cpio archive in the `newc` format (what initramfs uses), read from
/// `r` without unpacking anything; entries are matched without a leading `./` or `/`.
pub fn cpio_entry(r: &mut impl Read, name: &str) -> io::Result<Option<Vec<u8>>> {
    let want = name.trim_start_matches("./").trim_start_matches('/');
    let mut header = [0u8; 110];
    loop {
        match fill(r, &mut header)? {
            0 => return Ok(None),
            110 => {}
            _ => return Err(io::ErrorKind::UnexpectedEof.into()),
        }
        if !matches!(&header[..6], b"070701" | b"070702") {
            return Err(io::Error::other("not a newc cpio archive"));
        }
        let field = |i: usize| {
            std::str::from_utf8(&header[6 + i * 8..14 + i * 8])
                .ok()
                .and_then(|s| u64::from_str_radix(s, 16).ok())
                .ok_or_else(|| io::Error::other("bad cpio header"))
        };
        let (size, namesize) = (field(6)?, field(11)?);
        let pad = |n: u64| (4 - n % 4) % 4;
        let mut entry = vec![0u8; usize::try_from(namesize).map_err(io::Error::other)?];
        r.read_exact(&mut entry)?;
        skip(r, pad(110 + namesize))?;
        let entry = String::from_utf8_lossy(&entry);
        let entry = entry.trim_end_matches('\0');
        if entry == "TRAILER!!!" {
            return Ok(None);
        }
        if entry.trim_start_matches("./").trim_start_matches('/') == want {
            let mut body = vec![0u8; usize::try_from(size).map_err(io::Error::other)?];
            r.read_exact(&mut body)?;
            return Ok(Some(body));
        }
        skip(r, size + pad(size))?;
    }
}

/// Read until `buf` is full or the stream ends; how many bytes came.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn skip(r: &mut impl Read, n: u64) -> io::Result<()> {
    let skipped = io::copy(&mut r.take(n), &mut io::sink())?;
    if skipped < n {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// The decompressor for an archive starting with `magic`; `Some(None)` for an uncompressed one.
fn decompressor(magic: &[u8]) -> Option<Option<&'static str>> {
    match magic {
        [0x1f, 0x8b, ..] => Some(Some("gzip")),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Some(Some("zstd")),
        [0xfd, b'7', b'z', b'X', b'Z', ..] => Some(Some("xz")),
        [b'0', b'7', b'0', b'7', b'0', ..] => Some(None),
        _ => None,
    }
}

/// [`HostRead`] on this host; commands through `S`.
pub struct LinuxHost<S> {
    spawner: S,
}

impl<S: Spawner> LinuxHost<S> {
    /// Read this host, spawning commands through `spawner`.
    pub fn new(spawner: S) -> Self {
        Self { spawner }
    }
}

/// A child's stdout pipe as a [`Read`].
struct Pipe(Box<dyn platform::fs::File>);

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0
            .read(buf)
            .map_err(|e| io::Error::other(format!("{e:?}")))
    }
}

impl<S: Spawner> HostRead for LinuxHost<S> {
    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn stat(&self, path: &Path) -> io::Result<Stat> {
        std::fs::metadata(path).map(|m| Stat {
            uid: m.uid(),
            gid: m.gid(),
            perm: m.mode() & 0o7777,
            dev: m.dev(),
            rdev: m.rdev(),
        })
    }

    fn open_rw(&self, path: &Path) -> io::Result<()> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map(drop)
    }

    fn statfs_mib(&self, path: &Path) -> io::Result<(u64, u64)> {
        crate::capacity::statfs_total_free(path).map_err(|e| io::Error::other(e.to_string()))
    }

    fn euid(&self) -> u32 {
        std::fs::metadata("/proc/self").map_or(u32::MAX, |m| m.uid())
    }

    fn run(&self, argv: &[&str]) -> Result<String, String> {
        let (exe, args) = argv.split_first().ok_or("empty command")?;
        let mut cmd = Command::new(exe, "/").args(args);
        cmd.stdin = Stdio::Null;
        cmd.stdout = Stdio::Pipe;
        cmd.stderr = Stdio::Pipe;
        let mut child = self
            .spawner
            .spawn(&cmd)
            .map_err(|e| format!("spawn {exe}: {e:?}"))?;
        let drain = |pipe: Option<Box<dyn platform::fs::File>>| match pipe {
            Some(mut pipe) => read_all(pipe.as_mut()).unwrap_or_default(),
            None => Vec::new(),
        };
        let stdout = drain(child.take_stdout());
        let stderr = drain(child.take_stderr());
        match child.wait() {
            Ok(status) if status.success() => Ok(String::from_utf8_lossy(&stdout).into_owned()),
            Ok(_) => Err(String::from_utf8_lossy(&stderr).trim().to_owned()),
            Err(e) => Err(format!("wait {exe}: {e:?}")),
        }
    }

    fn archive_entry(&self, archive: &Path, name: &str) -> Result<Option<Vec<u8>>, String> {
        let fail = |e: io::Error| format!("{}: {e}", archive.display());
        let mut file = BufReader::new(std::fs::File::open(archive).map_err(fail)?);
        let magic = file
            .fill_buf()
            .map_err(fail)?
            .get(..6)
            .unwrap_or_default()
            .to_vec();
        match decompressor(&magic) {
            None => Err("unknown compression".into()),
            Some(None) => cpio_entry(&mut file, name).map_err(fail),
            Some(Some(tool)) => {
                let path = archive.to_string_lossy();
                let mut cmd = Command::new(tool, "/").args(["-dc", "--", path.as_ref()]);
                cmd.stdin = Stdio::Null;
                cmd.stdout = Stdio::Pipe;
                cmd.stderr = Stdio::Null;
                let mut child = self
                    .spawner
                    .spawn(&cmd)
                    .map_err(|e| format!("spawn {tool}: {e:?}"))?;
                let found = match child.take_stdout() {
                    Some(out) => cpio_entry(&mut BufReader::new(Pipe(out)), name).map_err(fail),
                    None => Err(format!("{tool}: no stdout")),
                };
                // The pipe is closed by now: a decompressor stopped early gets EPIPE and exits.
                let _ = child.wait();
                found
            }
        }
    }

    fn cmdlines(&self) -> Vec<Vec<String>> {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        dir.flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.parse::<u32>().is_ok())
            })
            .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
            .map(|raw| {
                raw.split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use ac_domain::tune::facts::Virt;

    /// A host made of fixture files.
    #[derive(Default)]
    struct Fixture {
        files: HashMap<PathBuf, String>,
        denied: Vec<PathBuf>,
        stats: HashMap<PathBuf, Stat>,
        euid: u32,
        kvm_denied: bool,
        commands: HashMap<String, Result<String, String>>,
        initrd: Option<Option<Vec<u8>>>,
    }

    impl Fixture {
        fn file(mut self, path: &str, text: &str) -> Self {
            self.files.insert(path.into(), text.into());
            self
        }

        fn stat(mut self, path: &str, s: Stat) -> Self {
            self.stats.insert(path.into(), s);
            self
        }
    }

    fn not_found() -> io::Error {
        io::ErrorKind::NotFound.into()
    }

    impl HostRead for Fixture {
        fn read(&self, path: &Path) -> io::Result<String> {
            if self.denied.iter().any(|d| d == path) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            self.files.get(path).cloned().ok_or_else(not_found)
        }
        fn stat(&self, path: &Path) -> io::Result<Stat> {
            self.stats.get(path).copied().ok_or_else(not_found)
        }
        fn open_rw(&self, _: &Path) -> io::Result<()> {
            if self.kvm_denied {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            Ok(())
        }
        fn statfs_mib(&self, _: &Path) -> io::Result<(u64, u64)> {
            Ok((15717, 15000))
        }
        fn euid(&self) -> u32 {
            self.euid
        }
        fn run(&self, argv: &[&str]) -> Result<String, String> {
            self.commands
                .get(&argv.join(" "))
                .cloned()
                .unwrap_or_else(|| Err("not found".into()))
        }
        fn archive_entry(&self, _: &Path, _: &str) -> Result<Option<Vec<u8>>, String> {
            self.initrd
                .clone()
                .ok_or_else(|| "unknown compression".into())
        }
        fn cmdlines(&self) -> Vec<Vec<String>> {
            Vec::new()
        }
    }

    const INITRD: &str = "/var/cache/kata-containers/kata-containers-initrd.img";

    fn file_stat(uid: u32, perm: u32) -> Stat {
        Stat {
            uid,
            gid: uid,
            perm,
            dev: 0,
            rdev: 0,
        }
    }

    /// BaileyServ: btrfs on sda3 of a rotational disk.
    fn baileyserv() -> Fixture {
        Fixture {
            initrd: Some(Some(
                format!("[Service]\nExecStartPre=-/usr/bin/busybox {INITRD_LOADS}\n").into_bytes(),
            )),
            ..Fixture::default()
        }
        .file("/proc/meminfo", "MemTotal:       24483408 kB\n")
        .file("/sys/devices/system/cpu/online", "0-3\n")
        .file(
            "/proc/cpuinfo",
            "flags\t\t: fpu vmx sse2\naddress sizes\t: 36 bits physical, 48 bits virtual\n",
        )
        .file(
            "/proc/self/mounts",
            "tmpfs /dev/shm tmpfs rw,seclabel,nosuid,nodev,size=16094208k,inode64,usrquota 0 0\n",
        )
        .file(
            "/proc/self/mountinfo",
            "42 1 0:34 /root / rw,relatime shared:1 - btrfs /dev/sda3 rw,seclabel,compress=zstd:1\n\
             41 39 0:26 / /dev/shm rw,nosuid,nodev shared:3 - tmpfs tmpfs rw,size=16094208k\n",
        )
        .file("/sys/dev/block/8:3/../queue/rotational", "1\n")
        .file(
            "/etc/fstab",
            "UUID=x / btrfs subvol=root 0 0\ntmpfs  /dev/shm  tmpfs  nosuid,nodev,inode64,usrquota,size=15717M  0  0\n",
        )
        .file(
            "/etc/kata-containers/configuration.toml",
            &format!(
                "[hypervisor.qemu]\ninitrd = \"{INITRD}\"\ndefault_vcpus = 2\ndefault_maxvcpus = 0\n\
                 default_memory = 2048\ndefault_maxmemory = 8192\n"
            ),
        )
        .file("/etc/agent-cloud/gateway.env", "AC_JOB_CPU_PSI_MAX=20\n")
        .stat("/var/lib/agent-cloud", Stat { dev: 36, ..file_stat(0, 0o755) })
        .stat("/dev/sda3", Stat { rdev: (8 << 8) | 3, ..file_stat(0, 0o660) })
        .stat("/dev/kvm", file_stat(0, 0o666))
        .stat(INITRD, file_stat(0, 0o644))
        .stat("/etc/agent-cloud/tokens", file_stat(0, 0o600))
        .stat("/etc/agent-cloud/egress-allow.txt", file_stat(0, 0o600))
    }

    fn paths() -> Paths {
        Paths::system("/etc/kata-containers/configuration.toml".into())
    }

    fn collected(h: &Fixture, scope: Scope) -> HostFacts {
        collect(h, &paths(), scope).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn collects_baileyserv_as_root() {
        let mut h = baileyserv();
        h.euid = 0;
        for (chain, n) in [
            ("inet agentfw input", 3),
            ("inet agentfw forward", 3),
            ("bridge agentfw forward", 1),
        ] {
            let listing =
                format!("table {chain} {{ # handle 1\n") + &"  rule # handle 9\n".repeat(n);
            h.commands
                .insert(format!("nft -a list chain {chain}"), Ok(listing));
        }
        for (what, out) in [
            ("interfaces", "acbr0\n"),
            ("ports", "3128/tcp\n"),
            ("services", "\n"),
        ] {
            h.commands.insert(
                format!("firewall-cmd --permanent --zone=agents --list-{what}"),
                Ok(out.into()),
            );
        }
        let f = collected(&h, Scope::Full);
        assert_eq!((f.mem_total_mib, f.cores), (23909, 4));
        assert_eq!(
            (f.phys_bits.clone(), f.virt.clone()),
            (Fact::Known(36), Fact::Known(Virt::Vmx))
        );
        assert_eq!(f.disk, Fact::Known(Disk::Rotational));
        assert_eq!(f.shm.known().map(|s| s.size_mib), Some(15717));
        assert_eq!(f.shm_fstab, f.shm);
        assert_eq!(f.shm_used_mib, Fact::Known(717));
        assert_eq!(f.kata.known().and_then(|k| k.default_maxmemory), Some(8192));
        assert_eq!(f.kvm, Fact::Known(Kvm::Usable));
        assert_eq!(f.initrd_net, Fact::Known(true));
        assert_eq!(
            f.firewall,
            Fact::Known(Firewall {
                chains: vec![Some(3), Some(3), Some(1)],
                zone: "acbr0|3128/tcp|".into()
            })
        );
        let root600 = Fact::Known(Mode {
            uid: 0,
            gid: 0,
            perm: 0o600,
        });
        assert_eq!(
            (f.tokens_mode.clone(), f.allowlist_mode.clone()),
            (root600.clone(), root600)
        );
        let fs = ac_domain::tune::recommend(&f, ac_domain::tune::Profile::Build);
        assert!(
            fs.iter()
                .all(|f| matches!(f.status, ac_domain::tune::Status::Ok) || f.rule == "disk.class"),
            "{fs:#?}"
        );
    }

    #[test]
    fn as_a_user_root_only_things_are_skipped() {
        let mut h = baileyserv();
        h.euid = 1000;
        h.denied.push("/etc/agent-cloud/gateway.env".into());
        let f = collected(&h, Scope::Full);
        assert!(
            matches!(&f.gateway, Fact::Skipped(why) if why.contains("gateway.env is root-only"))
        );
        assert!(matches!(&f.firewall, Fact::Skipped(why) if why.contains("need root")));
        // Settings only: no prerequisites read.
        let s = collected(&h, Scope::Settings);
        assert!(matches!(s.kvm, Fact::Skipped(_)) && matches!(s.initrd_net, Fact::Skipped(_)));
    }

    #[test]
    fn missing_and_broken_pieces() {
        let mut h = baileyserv();
        h.files
            .remove(Path::new("/etc/kata-containers/configuration.toml"));
        h.stats.remove(Path::new("/dev/kvm"));
        h.files
            .insert("/etc/fstab".into(), "UUID=x / btrfs defaults 0 0\n".into());
        let f = collected(&h, Scope::Full);
        assert_eq!(
            (f.kata.clone(), f.kvm, f.shm_fstab.clone()),
            (Fact::Absent, Fact::Absent, Fact::Absent)
        );
        assert!(matches!(f.initrd_net, Fact::Skipped(_)));
        let mut h = baileyserv();
        h.kvm_denied = true;
        h.initrd = Some(None);
        h.files.insert(
            "/etc/kata-containers/configuration.toml".into(),
            format!("[hypervisor.qemu]\ninitrd = \"{INITRD}\"\n"),
        );
        let f = collected(&h, Scope::Full);
        assert_eq!(
            (f.kvm, f.initrd_net),
            (Fact::Known(Kvm::NoAccess), Fact::Known(false))
        );
        h.files.insert(
            "/etc/kata-containers/configuration.toml".into(),
            "[hypervisor.qemu]\ndefault_memory = 1\ndefault_memory = 2\n".into(),
        );
        assert!(matches!(
            collected(&h, Scope::Settings).kata,
            Fact::Invalid(_)
        ));
        // Without its size the host cannot be judged at all.
        h.files.remove(Path::new("/proc/meminfo"));
        assert!(collect(&h, &paths(), Scope::Settings).is_err());
    }

    #[test]
    fn disk_class_on_a_plain_partition_and_whole_disk() {
        let h = baileyserv()
            .stat(
                "/var/lib/agent-cloud",
                Stat {
                    dev: (259 << 8) | 2,
                    ..file_stat(0, 0o755)
                },
            )
            .file("/sys/dev/block/259:2/queue/rotational", "0\n");
        assert_eq!(
            collected(&h, Scope::Settings).disk,
            Fact::Known(Disk::Solid)
        );
        // An anonymous device with no mount to name its disk: unknown, not guessed.
        let mut h = baileyserv();
        h.files.remove(Path::new("/proc/self/mountinfo"));
        assert!(matches!(
            collected(&h, Scope::Settings).disk,
            Fact::Skipped(_)
        ));
    }

    #[test]
    fn rooted_paths_move_only_the_edited_files() {
        let p = paths().rooted(Path::new("/tmp/x"));
        assert_eq!(p.fstab, PathBuf::from("/tmp/x/etc/fstab"));
        assert_eq!(
            p.kata,
            PathBuf::from("/tmp/x/etc/kata-containers/configuration.toml")
        );
        assert_eq!(
            p.gateway_env,
            PathBuf::from("/tmp/x/etc/agent-cloud/gateway.env")
        );
        assert_eq!(
            (p.proc, p.tokens),
            ("/proc".into(), "/etc/agent-cloud/tokens".into())
        );
    }

    #[test]
    fn device_numbers_and_mountinfo() {
        assert_eq!(major_minor(36), (0, 36));
        assert_eq!(major_minor((8 << 8) | 3), (8, 3));
        assert_eq!(major_minor((259 << 8) | (1 << 20) | 5), (259, (1 << 8) | 5));
        let mi = "22 42 8:1 / /boot rw - ext4 /dev/sda1 rw\n42 1 0:34 /root / rw shared:1 - btrfs \
                  /dev/sda3 rw\n50 42 0:40 / /var/lib/x rw - tmpfs tmpfs rw\n";
        let at = |p: &str| mount_source(mi, Path::new(p));
        assert_eq!(at("/var/lib/agent-cloud"), Some("/dev/sda3"));
        assert_eq!(at("/boot/efi"), Some("/dev/sda1"));
        assert_eq!(at("/var/lib/x/y"), Some("tmpfs"));
        // Component-wise: /var/lib/xy is not under /var/lib/x.
        assert_eq!(at("/var/lib/xy"), Some("/dev/sda3"));
        assert_eq!(mount_source("", Path::new("/")), None);
    }

    #[test]
    fn nft_listing_counts() {
        let listing = "table inet agentfw {\n\tchain input { # handle 1\n\t\ttype filter hook input priority filter; policy accept;\n\t\tiifname \"acbr0\" tcp dport 3128 accept # handle 4\n\t\tiifname \"acbr0\" reject # handle 5\n\t\tip saddr 10.0.0.0/8 drop # handle 6\n\t}\n}\n";
        assert_eq!(nft_rules(listing), 3);
        assert_eq!(nft_rules(""), 0);
    }

    #[test]
    fn sandboxes_from_cmdlines() {
        let argv = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        let procs = vec![
            argv("/usr/bin/containerd-shim-kata-v2 -namespace agents -address /run/c.sock -id a2"),
            argv("containerd-shim-kata-v2 -namespace agents -id a1"),
            argv("/usr/bin/containerd-shim-kata-v2 -namespace agents -id a1"),
            argv("grep containerd-shim-kata-v2 -id x"),
            argv("/usr/bin/containerd-shim-kata-v2 -namespace agents"),
        ];
        assert_eq!(kata_sandboxes(&procs), vec!["a1", "a2"]);
    }

    /// A newc archive of `(name, body)` entries.
    fn cpio(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut put = |name: &str, body: &[u8]| {
            let namesize = name.len() + 1;
            out.extend(
                format!(
                    "070701{}",
                    (0..13)
                        .map(|i| format!(
                            "{:08x}",
                            match i {
                                6 => body.len(),
                                11 => namesize,
                                _ => 0,
                            }
                        ))
                        .collect::<String>()
                )
                .into_bytes(),
            );
            out.extend(name.as_bytes());
            out.push(0);
            out.extend(std::iter::repeat_n(0, (4 - (110 + namesize) % 4) % 4));
            out.extend(body);
            out.extend(std::iter::repeat_n(0, (4 - body.len() % 4) % 4));
        };
        for (name, body) in entries {
            put(name, body);
        }
        put("TRAILER!!!", b"");
        out
    }

    #[test]
    fn cpio_entries() {
        let archive = cpio(&[
            (".", b""),
            ("usr/bin/kata-agent", b"\x7fELF..."),
            (&format!("./{INITRD_DROPIN}"), b"[Service]\nx\n"),
        ]);
        let found = cpio_entry(&mut archive.as_slice(), INITRD_DROPIN);
        assert_eq!(
            found.ok().flatten().as_deref(),
            Some(&b"[Service]\nx\n"[..])
        );
        assert_eq!(
            cpio_entry(&mut archive.as_slice(), "etc/none").ok(),
            Some(None)
        );
        assert!(cpio_entry(&mut &b"garbage that is not a cpio header at all, long enough to fill one header of 110 bytes......................"[..], "x").is_err());
        assert!(cpio_entry(&mut &archive[..200], INITRD_DROPIN).is_err());
        assert_eq!(decompressor(&[0x1f, 0x8b, 8, 0, 0, 0]), Some(Some("gzip")));
        assert_eq!(decompressor(b"070701"), Some(None));
        assert_eq!(decompressor(b"PK\x03\x04"), None);
    }
}
