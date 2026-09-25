//! Every side effect `deploy-gate` has outside its own directories:
//! processes, HTTP, the clock, free disk space and progress output. The
//! logic takes a `&dyn System`, so tests run it against a fake VM.

use super::http;
use std::ffi::CString;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const SYSTEMCTL: &str = "/usr/bin/systemctl";
pub const RUNUSER: &str = "/usr/sbin/runuser";
/// Children start with an empty environment plus this `PATH`, so nothing
/// from the SSH session or sudo leaks into them.
pub const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A process to run: an argv, never a shell line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// Run as this user through `runuser`, instead of as root.
    pub user: Option<String>,
    /// Added to the child's otherwise empty environment.
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

impl Cmd {
    pub fn new(program: impl AsRef<Path>, args: &[&str]) -> Cmd {
        Cmd {
            program: program.as_ref().to_string_lossy().into_owned(),
            args: args.iter().map(|a| a.to_string()).collect(),
            ..Cmd::default()
        }
    }

    pub fn as_user(mut self, user: &str) -> Cmd {
        self.user = Some(user.to_string());
        self
    }

    pub fn with_env(mut self, env: Vec<(String, String)>) -> Cmd {
        self.env = env;
        self
    }

    pub fn in_dir(mut self, dir: impl Into<PathBuf>) -> Cmd {
        self.cwd = Some(dir.into());
        self
    }

    /// For progress output. Never includes the environment, which can hold
    /// secrets.
    pub fn line(&self) -> String {
        let user = self
            .user
            .as_ref()
            .map(|u| format!("[{u}] "))
            .unwrap_or_default();
        let mut words = vec![self.program.as_str()];
        words.extend(self.args.iter().map(String::as_str));
        format!("{user}{}", words.join(" "))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: &'static str,
    pub addr: SocketAddr,
    pub path: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<String>,
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

pub trait System {
    fn run(&self, cmd: &Cmd) -> io::Result<Output>;
    fn http(&self, request: &Request) -> io::Result<Response>;
    /// Unix seconds.
    fn now(&self) -> u64;
    fn sleep(&self, duration: Duration);
    /// Bytes an unprivileged user could still write on `path`'s filesystem.
    fn free_bytes(&self, path: &Path) -> io::Result<u64>;
    /// One line of human-readable progress (stderr).
    fn say(&self, line: &str);
}

/// The real VM.
pub struct RealSystem;

impl System for RealSystem {
    fn run(&self, cmd: &Cmd) -> io::Result<Output> {
        let out = command(cmd).output()?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn http(&self, request: &Request) -> io::Result<Response> {
        http::send(request)
    }

    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn free_bytes(&self, path: &Path) -> io::Result<u64> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        let mut stat = MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: `path` is NUL-terminated and outlives the call, and
        // `stat` points to writable memory of the right type.
        if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: statvfs returned 0, so it filled in `stat`.
        let stat = unsafe { stat.assume_init() };
        Ok(blocks_to_bytes(stat.f_bavail, stat.f_frsize))
    }

    /// Never panics: once CI's SSH channel closes, stderr fails with
    /// EPIPE, and a panic then could leave the units stopped.
    fn say(&self, line: &str) {
        let _ = writeln!(io::stderr(), "{line}");
    }
}

/// The field types differ by platform (`u32` blocks on macOS, `u64` on
/// Linux), hence `From` on values that are already `u64` on Linux.
#[allow(clippy::useless_conversion)]
fn blocks_to_bytes(blocks: libc::fsblkcnt_t, block_size: libc::c_ulong) -> u64 {
    u64::from(blocks).saturating_mul(u64::from(block_size))
}

/// The `std` command for `cmd`: `runuser -u USER -- program args...` when
/// it names a user, an empty environment plus `PATH` and `cmd.env`, and no
/// stdin.
pub fn command(cmd: &Cmd) -> Command {
    let mut command = match &cmd.user {
        Some(user) => {
            let mut c = Command::new(RUNUSER);
            c.args(["-u", user, "--", &cmd.program]);
            c
        }
        None => Command::new(&cmd.program),
    };
    command
        .args(&cmd.args)
        .env_clear()
        .env("PATH", PATH)
        .envs(cmd.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null());
    if let Some(dir) = &cmd.cwd {
        command.current_dir(dir);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::testing::TempRoot;
    use std::ffi::OsStr;

    #[test]
    fn a_command_runs_as_argv_with_only_the_given_environment() {
        let tmp = TempRoot::new();
        let cmd = Cmd::new("/usr/bin/env", &[])
            .with_env(vec![("A".into(), "1 2; $(x)".into())])
            .in_dir(tmp.path());
        let out = RealSystem.run(&cmd).unwrap();
        assert!(out.success);
        let mut lines: Vec<&str> = out.stdout.lines().collect();
        lines.sort();
        assert_eq!(lines, ["A=1 2; $(x)", &format!("PATH={PATH}")]);

        let pwd = RealSystem
            .run(&Cmd::new("/bin/pwd", &["-P"]).in_dir(tmp.path()))
            .unwrap();
        let expected = tmp.path().canonicalize().unwrap();
        assert_eq!(pwd.stdout.trim(), expected.to_str().unwrap());
    }

    #[test]
    fn a_failing_or_missing_program_is_reported() {
        let out = RealSystem
            .run(&Cmd::new("/bin/ls", &["/no/such/path"]))
            .unwrap();
        assert!(!out.success);
        assert!(!out.stderr.is_empty());
        assert!(RealSystem.run(&Cmd::new("/no/such/program", &[])).is_err());
    }

    #[test]
    fn a_user_runs_the_program_through_runuser() {
        let cmd = Cmd::new("/opt/x/athena", &["backup", "/b.db"]).as_user("athena");
        let command = command(&cmd);
        assert_eq!(command.get_program(), OsStr::new(RUNUSER));
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            ["-u", "athena", "--", "/opt/x/athena", "backup", "/b.db"]
        );
        assert_eq!(command.get_current_dir(), None);
        assert_eq!(cmd.line(), "[athena] /opt/x/athena backup /b.db");
    }

    #[test]
    fn the_clock_and_disk_are_real() {
        let before = RealSystem.now();
        RealSystem.sleep(Duration::from_millis(1));
        assert!(RealSystem.now() >= before && before > 1_700_000_000);

        let tmp = TempRoot::new();
        assert!(RealSystem.free_bytes(tmp.path()).unwrap() > 0);
        let missing = RealSystem
            .free_bytes(&tmp.path().join("missing"))
            .unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        let nul = RealSystem.free_bytes(Path::new(OsStr::from_bytes(b"a\0b")));
        assert_eq!(nul.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        RealSystem.say("deploy-gate sys test: progress goes to stderr");

        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let request = Request {
            method: "GET",
            addr: closed,
            path: "/health".into(),
            headers: vec![],
            body: None,
            timeout: Duration::from_secs(1),
        };
        assert!(RealSystem.http(&request).is_err());
    }
}
