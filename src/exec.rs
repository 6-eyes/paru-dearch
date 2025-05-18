use crate::args::Args;
use crate::config::Config;

use std::ffi::OsStr;
use std::fmt::{Debug, Display, Formatter};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::debug;
use signal_hook::consts::signal::*;
use signal_hook::flag as signal_flag;
use std::sync::LazyLock;
use tr::tr;

/// - Overrides the OS handlers for the signals `SIGTERM`, `SIGINT` and `SIGQUIT`.
/// - This means that on any of these signals, instead of OS, the crate will execute similar process (default) on program execution.
/// - This default behavior is only executed if the condition (`AtomicBool`) is set to true.
/// - Since this condition is wrapped in *Arc*, the instance is shared by all the default handlers. Also any thread can set the value to false to neglect kernel signals.
pub(crate) static DEFAULT_SIGNALS: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| {
    let arc = Arc::new(AtomicBool::new(true));
    signal_flag::register_conditional_default(SIGTERM, Arc::clone(&arc)).unwrap();
    signal_flag::register_conditional_default(SIGINT, Arc::clone(&arc)).unwrap();
    signal_flag::register_conditional_default(SIGQUIT, Arc::clone(&arc)).unwrap();
    arc
});

static CAUGHT_SIGNAL: LazyLock<Arc<AtomicUsize>> = LazyLock::new(|| {
    let arc = Arc::new(AtomicUsize::new(0));
    signal_flag::register_usize(SIGTERM, Arc::clone(&arc), SIGTERM as usize).unwrap();
    signal_flag::register_usize(SIGINT, Arc::clone(&arc), SIGINT as usize).unwrap();
    signal_flag::register_usize(SIGQUIT, Arc::clone(&arc), SIGQUIT as usize).unwrap();
    arc
});

pub static RAISE_SIGPIPE: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| {
    let arc = Arc::new(AtomicBool::new(true));
    signal_flag::register_conditional_default(SIGPIPE, Arc::clone(&arc)).unwrap();
    arc
});

#[derive(Debug, Clone, Copy)]
pub struct Status(pub i32);

impl Display for Status {
    fn fmt(&self, _f: &mut Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

impl std::error::Error for Status {}

impl Status {
    pub fn code(self) -> i32 {
        self.0
    }

    /// Returns [`Ok`] if the status value is 0.
    pub fn success(self) -> Result<i32, Status> {
        (self.0 == 0).then(|| 0).ok_or(self)
    }
}

/// Generates error string indicating the command executed as child process.
#[inline]
fn command_err(cmd: &Command) -> String {
    format!("{} {} {}", tr!("failed to run:"), cmd.get_program().to_string_lossy(), cmd.get_args().map(OsStr::to_string_lossy).collect::<Vec<_>>().join(" "))
}

/// Executes the child process returning its executuon status
fn command_status(cmd: &mut Command) -> Result<Status> {
    debug!("running command: {:?}", cmd);
    let term = &*CAUGHT_SIGNAL;

    // ignore system signals
    DEFAULT_SIGNALS.store(false, Ordering::Relaxed);

    let ret = cmd
        .status()
        .map(|s| Status(s.code().unwrap_or(1)))
        .with_context(|| command_err(cmd));

    // restore default handlers
    DEFAULT_SIGNALS.store(true, Ordering::Relaxed);

    // check on any signals caught while execution of child process.
    match term.swap(0, Ordering::Relaxed) {
        0 => ret,
        n => std::process::exit(128 + n as i32),
    }
}

/// Executes the given command
pub fn command(cmd: &mut Command) -> Result<()> {
    debug!("running command: {:?}", cmd);
    command_status(cmd)?.success()?;
    Ok(())
}

pub fn command_output(cmd: &mut Command) -> Result<Output> {
    debug!("running command: {:?}", cmd);
    let term = &*CAUGHT_SIGNAL;

    DEFAULT_SIGNALS.store(false, Ordering::Relaxed);

    let ret = cmd.output().with_context(|| command_err(cmd));

    DEFAULT_SIGNALS.store(true, Ordering::Relaxed);
    let ret = match term.swap(0, Ordering::Relaxed) {
        0 => ret?,
        n => std::process::exit(128 + n as i32),
    };

    if !ret.status.success() {
        bail!(
            "{}: {}",
            command_err(cmd),
            String::from_utf8_lossy(&ret.stderr).trim()
        );
    }

    Ok(ret)
}

pub fn spawn_sudo(sudo: String, flags: Vec<String>) -> Result<()> {
    update_sudo(&sudo, &flags)?;
    thread::spawn(move || sudo_loop(&sudo, &flags));
    Ok(())
}

fn sudo_loop<S: AsRef<OsStr>>(sudo: &str, flags: &[S]) -> Result<()> {
    loop {
        thread::sleep(Duration::from_secs(250));
        update_sudo(sudo, flags)?;
    }
}

fn update_sudo<S: AsRef<OsStr>>(sudo: &str, flags: &[S]) -> Result<()> {
    let mut cmd = Command::new(sudo);
    cmd.args(flags);
    let status = command_status(&mut cmd)?;
    status.success()?;
    Ok(())
}

/// sleep while pacman is in use
async fn wait_for_lock(config: &Config) {
    let path = Path::new(config.alpm.dbpath()).join("db.lck");
    let c = config.color;
    if path.exists() {
        println!("{} {}", c.error.paint("::"), c.bold.paint(tr!("Pacman is currently in use, please wait...")));

        while path.exists() {
            tokio::time::sleep(Duration::from_secs(3)).await;
            // std::thread::sleep(Duration::from_secs(3));
        }
    }
}

async fn new_pacman<S: AsRef<str> + Display + Debug>(config: &Config, args: &Args<S>) -> Command {
    let mut cmd = if config.need_root {
        // upgrade
        wait_for_lock(config).await;
        let mut cmd = Command::new(&config.sudo_bin);
        cmd.args(&config.sudo_flags).arg(args.bin.as_ref());
        cmd
    }
    else {
        Command::new(args.bin.as_ref())
    };

    if let Some(config) = &config.pacman_conf {
        cmd.args(["--config", config]);
    }
    cmd.args(args.args());
    cmd
}

pub async fn pacman<S: AsRef<str> + Display + Debug>(config: &Config, args: &Args<S>) -> Result<Status> {
    let mut cmd = new_pacman(config, args).await;
    command_status(&mut cmd)
}

pub async fn pacman_output<S: AsRef<str> + Display + std::fmt::Debug>(
    config: &Config,
    args: &Args<S>,
) -> Result<Output> {
    let mut cmd = new_pacman(config, args).await;
    cmd.stdin(Stdio::inherit());
    command_output(&mut cmd)
}

fn new_makepkg<S: AsRef<OsStr>>(
    config: &Config,
    dir: &Path,
    args: &[S],
    pkgdest: Option<&str>,
) -> Command {
    let mut cmd = Command::new(&config.makepkg_bin);
    if let Some(mconf) = &config.makepkg_conf {
        cmd.arg("--config").arg(mconf);
    }
    if let Some(dest) = pkgdest {
        cmd.env("PKGDEST", dest);
    }
    cmd.args(&config.mflags).args(args).current_dir(dir);
    cmd
}

pub fn makepkg_dest<S: AsRef<OsStr>>(
    config: &Config,
    dir: &Path,
    args: &[S],
    pkgdest: Option<&str>,
) -> Result<Status> {
    let mut cmd = new_makepkg(config, dir, args, pkgdest);
    command_status(&mut cmd)
}

pub fn makepkg<S: AsRef<OsStr>>(config: &Config, dir: &Path, args: &[S]) -> Result<Status> {
    makepkg_dest(config, dir, args, None)
}

pub fn makepkg_output_dest<S: AsRef<OsStr>>(
    config: &Config,
    dir: &Path,
    args: &[S],
    pkgdest: Option<&str>,
) -> Result<Output> {
    let mut cmd = new_makepkg(config, dir, args, pkgdest);
    command_output(&mut cmd)
}

pub fn makepkg_output<S: AsRef<OsStr>>(config: &Config, dir: &Path, args: &[S]) -> Result<Output> {
    makepkg_output_dest(config, dir, args, None)
}
