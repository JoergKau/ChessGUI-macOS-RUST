//! Checks a chosen file before an engine is started, and the environment an
//! engine runs in.
//!
//! The user may point at a program, or at a launcher script that starts one
//! (a shell script, a Java launcher, a Python wrapper). Both are accepted. A
//! file that cannot run on this Mac is refused with the reason, before any
//! process is started. Engines also run with the usual tool folders on `PATH`
//! (Finder gives an app only a minimal one), and from their own folder, since
//! many engines load their data files relative to it.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::engine::protocol::EngineError;

/// Folders an engine may need on `PATH` that a Finder launch does not have.
const TOOL_FOLDERS: [&str; 6] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

/// How many recent stderr lines are kept for a failure report.
const STDERR_KEPT: usize = 5;

/// Reject anything that cannot be started as an engine on this Mac, with the
/// reason. A launcher script passes when its interpreter exists; whether the
/// file really speaks UCI is decided by the handshake.
pub fn check_executable(binary: &Path) -> Result<(), EngineError> {
    let fail = |reason: String| EngineError::SpawnFailed {
        binary: Some(binary.to_path_buf()),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, reason),
    };
    let metadata = std::fs::metadata(binary).map_err(|source| EngineError::SpawnFailed {
        binary: Some(binary.to_path_buf()),
        source,
    })?;
    if !metadata.is_file() {
        return Err(fail("not a regular file".to_string()));
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(fail("file is not marked executable".to_string()));
    }
    let head = read_head(binary).map_err(|source| EngineError::SpawnFailed {
        binary: Some(binary.to_path_buf()),
        source,
    })?;
    match program_kind(&head) {
        ProgramKind::Script => check_interpreter(&head).map_err(fail),
        ProgramKind::MachO => Ok(()),
        ProgramKind::Foreign(what) => Err(fail(format!("it is {what}, which this Mac cannot run"))),
        ProgramKind::Unknown => Err(fail("it is not a program this Mac can run".to_string())),
    }
}

/// What the first bytes of a file say about it.
#[derive(Debug, PartialEq, Eq)]
enum ProgramKind {
    /// A `#!` script; its interpreter is checked separately.
    Script,
    /// A native macOS program (thin or universal).
    MachO,
    /// A program or archive for another system, with a short name for it.
    Foreign(&'static str),
    /// Anything else (a text file, a document, a picture).
    Unknown,
}

/// Classify a file by its first bytes.
fn program_kind(head: &[u8]) -> ProgramKind {
    const MACH_O: [[u8; 4]; 4] = [
        [0xCF, 0xFA, 0xED, 0xFE],
        [0xCE, 0xFA, 0xED, 0xFE],
        [0xCA, 0xFE, 0xBA, 0xBE],
        [0xBE, 0xBA, 0xFE, 0xCA],
    ];
    if head.starts_with(b"#!") {
        ProgramKind::Script
    } else if head.len() >= 4 && MACH_O.iter().any(|magic| head[..4] == *magic) {
        ProgramKind::MachO
    } else if head.starts_with(b"\x7fELF") {
        ProgramKind::Foreign("a Linux program")
    } else if head.starts_with(b"MZ") {
        ProgramKind::Foreign("a Windows program")
    } else if head.starts_with(b"PK\x03\x04") {
        ProgramKind::Foreign("a Java archive or zip file; choose the script that starts it")
    } else {
        ProgramKind::Unknown
    }
}

/// The interpreter named on a script's `#!` line must exist. For `env`, the
/// program it starts must be on the engine search path.
fn check_interpreter(head: &[u8]) -> Result<(), String> {
    let first_line = head.split(|&byte| byte == b'\n').next().unwrap_or_default();
    let line = String::from_utf8_lossy(&first_line[2..]).into_owned();
    let mut words = line.split_whitespace();
    let interpreter = words
        .next()
        .ok_or_else(|| "the script's #! line is empty".to_string())?;
    let program = if Path::new(interpreter)
        .file_name()
        .is_some_and(|name| name == "env")
    {
        words
            .find(|word| !word.starts_with('-'))
            .ok_or_else(|| "the script's #! line names no program after env".to_string())?
    } else {
        interpreter
    };
    if Path::new(program).is_absolute() {
        if Path::new(program).exists() {
            Ok(())
        } else {
            Err(format!("the script runs {program}, which does not exist"))
        }
    } else if find_on_engine_path(program).is_some() {
        Ok(())
    } else {
        Err(format!(
            "the script runs {program}, which is not on the engine's search path"
        ))
    }
}

/// The first bytes of a file, enough to recognise its kind.
fn read_head(binary: &Path) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(512);
    std::fs::File::open(binary)?
        .take(512)
        .read_to_end(&mut head)?;
    Ok(head)
}

/// Find an executable named `program` in the engine search path.
fn find_on_engine_path(program: &str) -> Option<PathBuf> {
    let path = engine_path();
    std::env::split_paths(&path)
        .map(|folder| folder.join(program))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// The `PATH` an engine runs with: the app's own, then the usual tool folders.
pub fn engine_path() -> OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let mut folders: Vec<PathBuf> = Vec::new();
    for folder in std::env::split_paths(&inherited).chain(TOOL_FOLDERS.iter().map(PathBuf::from)) {
        if !folder.as_os_str().is_empty() && !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    std::env::join_paths(folders).unwrap_or_default()
}

/// What an engine has said about its own failure: the last stderr lines, and
/// how the process ended. Shown when an engine does not start.
#[derive(Debug, Default)]
pub struct Diagnostics {
    /// The most recent stderr lines, oldest first.
    stderr: VecDeque<String>,
    /// How the process ended, once it has.
    exit: Option<String>,
}

impl Diagnostics {
    /// Keep one stderr line.
    pub fn note_stderr(&mut self, line: &str) {
        if self.stderr.len() == STDERR_KEPT {
            self.stderr.pop_front();
        }
        self.stderr.push_back(line.to_string());
    }

    /// Record how the process ended.
    pub fn note_exit(&mut self, status: String) {
        self.exit = Some(status);
    }

    /// A short report for the status line: the exit status and the last
    /// message, or a note that the engine said nothing.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(exit) = &self.exit {
            parts.push(exit.clone());
        }
        match self.stderr.back() {
            Some(last) => parts.push(format!("last message: {last}")),
            None => parts.push("it printed no message".to_string()),
        }
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn a_java_archive_or_a_foreign_program_is_refused_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let jar = write_file(dir.path(), "engine.jar", b"PK\x03\x04rest", 0o755);
        let err = check_executable(&jar).unwrap_err().to_string();
        assert!(err.contains("Java archive"), "{err}");
        let linux = write_file(dir.path(), "engine-linux", b"\x7fELF\x02\x01", 0o755);
        assert!(check_executable(&linux)
            .unwrap_err()
            .to_string()
            .contains("Linux program"));
        let windows = write_file(dir.path(), "engine.exe", b"MZ\x90\x00", 0o755);
        assert!(check_executable(&windows)
            .unwrap_err()
            .to_string()
            .contains("Windows program"));
        let text = write_file(dir.path(), "notes", b"just words\n", 0o755);
        assert!(check_executable(&text)
            .unwrap_err()
            .to_string()
            .contains("not a program"));
    }

    #[test]
    fn a_script_whose_interpreter_is_missing_is_refused_with_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_file(
            dir.path(),
            "wrapper.sh",
            b"#!/opt/nowhere/python\nprint(1)\n",
            0o755,
        );
        let err = check_executable(&script).unwrap_err().to_string();
        assert!(err.contains("/opt/nowhere/python"), "{err}");
    }

    #[test]
    fn a_script_started_through_env_needs_its_program_on_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_file(dir.path(), "env.sh", b"#!/usr/bin/env sh\necho hi\n", 0o755);
        assert!(
            check_executable(&script).is_ok(),
            "sh is on every engine path"
        );
        let missing = write_file(
            dir.path(),
            "missing.sh",
            b"#!/usr/bin/env no-such-engine-tool\n",
            0o755,
        );
        assert!(check_executable(&missing)
            .unwrap_err()
            .to_string()
            .contains("no-such-engine-tool"));
    }

    #[test]
    fn a_native_program_passes_the_check() {
        assert!(check_executable(Path::new("/bin/ls")).is_ok());
    }

    #[test]
    fn the_engine_path_keeps_the_app_path_and_adds_the_tool_folders() {
        let path = engine_path();
        let folders: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert!(folders.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert!(folders.contains(&PathBuf::from("/usr/bin")));
        for tool in TOOL_FOLDERS {
            let count = folders
                .iter()
                .filter(|folder| folder.as_os_str() == tool)
                .count();
            assert!(count <= 1, "{tool} is listed {count} times");
        }
    }

    #[test]
    fn the_failure_report_gives_the_exit_status_and_the_last_message() {
        let mut diagnostics = Diagnostics::default();
        assert_eq!(diagnostics.summary(), "it printed no message");
        for n in 0..7 {
            diagnostics.note_stderr(&format!("line {n}"));
        }
        diagnostics.note_exit("exit status: 127".to_string());
        let summary = diagnostics.summary();
        assert!(
            summary.starts_with("exit status: 127; last message: line 6"),
            "{summary}"
        );
        assert!(
            !diagnostics.stderr.iter().any(|line| line == "line 0"),
            "only the newest lines are kept"
        );
    }
}
