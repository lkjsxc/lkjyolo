use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

pub fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path == Path::new("~") {
        home()?
    } else if let Ok(relative) = path.strip_prefix("~/") {
        home()?.join(relative)
    } else {
        path.to_path_buf()
    };
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}

pub fn private_dir(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            !meta.file_type().is_symlink(),
            "State directory must not be a symlink: {}",
            path.display()
        );
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let meta = fs::metadata(path)?;
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() },
        "State directory is not owned by this user: {}",
        path.display()
    );
    ensure!(
        meta.mode() & 0o077 == 0,
        "State directory must have mode 0700: {}",
        path.display()
    );
    Ok(())
}

pub fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().context("Missing parent directory")?;
    let temporary = parent.join(format!(".lkjyolo-{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');
    atomic_write(path, &data, 0o600)
}

/// Closing the descriptor releases flock, including on early return or panic.
pub fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.lock_exclusive()?;
    Ok(file)
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let count = file.read(&mut buf)?;
        if count == 0 {
            break;
        }
        digest.update(&buf[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn fingerprint(pid: u32) -> Option<String> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = raw.get(raw.rfind(')')? + 2..)?.split_whitespace().collect();
    if *fields.first()? == "Z" {
        return None;
    }
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(format!("{}:{}", boot.trim(), fields.get(19)?))
}

pub fn detached(command: &mut Command) -> &mut Command {
    // Only an async-signal-safe syscall runs between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

pub fn wait_child(child: &mut Child, seconds: u64) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Process timed out after {seconds} seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn append_log(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?)
}
