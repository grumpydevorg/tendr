//! Start a supervised child with raw `CreateProcessW`, suspended, so that it
//! is in its Job Object before it runs its first instruction: a child that
//! starts descendants at once cannot place one outside the job.
//!
//! `std::process::Command` cannot do this: it hands back no thread to
//! resume, and takes no pseudoconsole. So this module builds the call
//! itself, with std's rules for finding the program, quoting the command
//! line (batch files included) and building the environment
//! (`library/std/src/sys/process/windows.rs` and `sys/args/windows.rs`,
//! Rust 1.98), so a session's `argv` means what it meant under `Command`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::HANDLE;

/// How the child's standard streams are connected.
pub(super) enum Streams {
    /// Inheritable handles for stdin, stdout and stderr. They are the only
    /// handles the child inherits.
    Handles([HANDLE; 3]),
    /// A pseudoconsole the child attaches to, whose handles become its
    /// stdio. It inherits no handles, and shares the process group of the
    /// console's other processes, so Ctrl+C typed into it reaches it.
    PseudoConsole(windows_sys::Win32::System::Console::HPCON),
}

/// A child created suspended: its main thread has not run yet.
pub(super) struct Suspended {
    pub(super) process: OwnedHandle,
    pub(super) thread: OwnedHandle,
    pub(super) pid: u32,
}

/// The environment a child starts with: tendr's own, less `env_remove`,
/// plus `env`. Names compare without case, as Windows does.
pub(super) struct Environment {
    vars: Vec<(OsString, OsString)>,
    path_changed: bool,
}

impl Environment {
    pub(super) fn inherited(env_remove: &BTreeSet<String>, env: &BTreeMap<String, String>) -> Self {
        let mut this = Self {
            vars: std::env::vars_os().collect(),
            path_changed: false,
        };
        for name in env_remove {
            this.remove(name);
        }
        for (name, value) in env {
            this.set(name, value.into());
        }
        this
    }

    pub(super) fn set(&mut self, name: &str, value: OsString) {
        self.remove(name);
        self.vars.push((name.into(), value));
    }

    fn remove(&mut self, name: &str) {
        let key = fold(OsStr::new(name));
        self.path_changed |= key == "PATH";
        self.vars.retain(|(k, _)| fold(k) != key);
    }

    /// The child's `PATH`, when the launch spec changed it: std searches it
    /// for the program first.
    fn child_path(&self) -> Option<&OsStr> {
        if !self.path_changed {
            return None;
        }
        self.vars
            .iter()
            .find(|(k, _)| fold(k) == "PATH")
            .map(|(_, v)| v.as_os_str())
    }

    /// The Unicode environment block: `NAME=value\0` entries sorted by name
    /// without case, then a final `\0`.
    fn block(&self) -> io::Result<Vec<u16>> {
        let mut vars: Vec<&(OsString, OsString)> = self.vars.iter().collect();
        vars.sort_by_cached_key(|(k, _)| fold(k));
        let mut block = Vec::new();
        if vars.is_empty() {
            block.push(0);
        }
        for (name, value) in vars {
            block.extend(no_nuls(name)?.encode_wide());
            block.push(u16::from(b'='));
            block.extend(no_nuls(value)?.encode_wide());
            block.push(0);
        }
        block.push(0);
        Ok(block)
    }
}

/// An environment variable name as Windows compares it.
fn fold(name: &OsStr) -> String {
    name.to_string_lossy().to_uppercase()
}

/// Create `argv` suspended, connected by `streams` (in a new process group
/// unless on a pseudoconsole). The caller resumes `thread` once the process
/// is in its job.
pub(super) fn spawn_suspended(
    argv: &[String],
    cwd: Option<&Path>,
    environment: &Environment,
    streams: &Streams,
) -> io::Result<Suspended> {
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        EXTENDED_STARTUPINFO_PRESENT, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
        STARTUPINFOEXW,
    };

    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
    let program = OsStr::new(program);
    let resolved = resolve_program(program, environment.child_path())?;
    let (application, mut command_line) = if is_batch_file(&resolved)? {
        (command_prompt()?, batch_command_line(&resolved, args)?)
    } else {
        (resolved, command_line(program, args)?)
    };
    command_line.push(0);
    let env_block = environment.block()?;
    let cwd = cwd.map(current_directory).transpose()?;

    let mut attributes = AttributeList::new()?;
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    // Under STARTF_USESTDHANDLES, null std handles on a pseudoconsole give
    // the child the console's, not the ones tendr's own stdio would hand it.
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    let (inherit, group) = match streams {
        Streams::Handles(handles) => {
            // SAFETY: `handles` outlives the CreateProcessW call below.
            unsafe {
                attributes.update(
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast(),
                    std::mem::size_of_val(handles),
                )?;
            }
            [
                si.StartupInfo.hStdInput,
                si.StartupInfo.hStdOutput,
                si.StartupInfo.hStdError,
            ] = *handles;
            (true, CREATE_NEW_PROCESS_GROUP)
        }
        Streams::PseudoConsole(hpc) => {
            // SAFETY: the attribute takes the HPCON value itself, which the
            // caller keeps open across the CreateProcessW call.
            unsafe {
                attributes.update(
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    *hpc as *const core::ffi::c_void,
                    std::mem::size_of_val(hpc),
                )?;
            }
            (false, 0)
        }
    };
    si.lpAttributeList = attributes.as_ptr();

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: every buffer is NUL-terminated and outlives the call; the
    // command line is mutable as CreateProcessW requires; `si` and its
    // attribute list are initialised above.
    let ok = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            i32::from(inherit),
            CREATE_SUSPENDED | group | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            env_block.as_ptr().cast(),
            cwd.as_ref().map_or(std::ptr::null(), |dir| dir.as_ptr()),
            &si.StartupInfo,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateProcessW succeeded, so both handles are valid and ours.
    Ok(unsafe {
        Suspended {
            process: OwnedHandle::from_raw_handle(pi.hProcess),
            thread: OwnedHandle::from_raw_handle(pi.hThread),
            pid: pi.dwProcessId,
        }
    })
}

/// An initialised `PROC_THREAD_ATTRIBUTE_LIST` with room for one attribute,
/// deleted on drop.
struct AttributeList(Vec<usize>);

impl AttributeList {
    fn new() -> io::Result<Self> {
        use windows_sys::Win32::System::Threading::InitializeProcThreadAttributeList;
        let mut size = 0;
        // SAFETY: a null list asks for the size only; this call "fails" with
        // ERROR_INSUFFICIENT_BUFFER by design.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        // usize-aligned storage, as the opaque list needs pointer alignment.
        let mut list = Self(vec![0; size.div_ceil(std::mem::size_of::<usize>())]);
        // SAFETY: the buffer holds at least `size` bytes.
        if unsafe { InitializeProcThreadAttributeList(list.as_ptr(), 1, 0, &mut size) } == 0 {
            let err = io::Error::last_os_error();
            // Nothing to delete: the list was never initialised.
            std::mem::take(&mut list.0);
            return Err(err);
        }
        Ok(list)
    }

    fn as_ptr(&mut self) -> windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    /// # Safety
    ///
    /// `value` must stay valid until the list is last used.
    unsafe fn update(
        &mut self,
        attribute: usize,
        value: *const core::ffi::c_void,
        size: usize,
    ) -> io::Result<()> {
        use windows_sys::Win32::System::Threading::UpdateProcThreadAttribute;
        // SAFETY: the list is initialised; the caller vouches for `value`.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                self.as_ptr(),
                0,
                attribute,
                value,
                size,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            // SAFETY: the list was initialised by `new`.
            unsafe {
                windows_sys::Win32::System::Threading::DeleteProcThreadAttributeList(self.as_ptr())
            };
        }
    }
}

/// Find the program as std does: a path is taken as given (with `.exe`
/// appended when that file exists); a bare name is searched for, with
/// `.exe` appended when it has no extension, in the child's changed `PATH`,
/// tendr's own directory, the system directories, then tendr's `PATH`.
/// Returns the NUL-terminated path.
fn resolve_program(program: &OsStr, child_path: Option<&OsStr>) -> io::Result<Vec<u16>> {
    let bytes = program.as_encoded_bytes();
    let is_separator = |b: u8| b == b'\\' || b == b'/';
    if bytes.last().is_none_or(|&b| is_separator(b)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "program path has no file name",
        ));
    }
    if bytes.iter().copied().any(is_separator) {
        if bytes.len() >= 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".exe") {
            return wide(program);
        }
        let mut with_exe = program.to_os_string();
        with_exe.push(".exe");
        return match exists(Path::new(&with_exe)) {
            Some(path) => Ok(path),
            None => wide(program),
        };
    }
    no_nuls(program)?;
    let has_extension = bytes.contains(&b'.');
    let candidate = |dir: PathBuf| {
        let mut path = dir.join(program);
        if !has_extension {
            path.set_extension("exe");
        }
        exists(&path)
    };
    let split = |paths: &OsStr| {
        std::env::split_paths(paths)
            .filter(|p| !p.as_os_str().is_empty())
            .collect::<Vec<_>>()
    };
    let mut dirs: Vec<PathBuf> = child_path.map(split).unwrap_or_default();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        dirs.push(dir);
    }
    dirs.extend(system_directory());
    dirs.extend(windows_directory());
    dirs.extend(
        std::env::var_os("PATH")
            .as_deref()
            .map(split)
            .unwrap_or_default(),
    );
    dirs.into_iter()
        .find_map(candidate)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "program not found"))
}

/// `path` NUL-terminated, if a file (or link) is there.
fn exists(path: &Path) -> Option<Vec<u16>> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
    let path = wide(path.as_os_str()).ok()?;
    // SAFETY: `path` is NUL-terminated.
    (unsafe { GetFileAttributesW(path.as_ptr()) } != INVALID_FILE_ATTRIBUTES).then_some(path)
}

fn system_directory() -> Option<PathBuf> {
    // SAFETY: the buffer and its length are passed together.
    fill(|buf, len| unsafe {
        windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(buf, len)
    })
    .map(|dir| OsString::from_wide(&dir).into())
}

fn windows_directory() -> Option<PathBuf> {
    // SAFETY: the buffer and its length are passed together.
    fill(|buf, len| unsafe {
        windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW(buf, len)
    })
    .map(|dir| OsString::from_wide(&dir).into())
}

/// Call a Win32 "fill this buffer" function, growing the buffer until the
/// result fits. `None` when it fails.
fn fill(mut call: impl FnMut(*mut u16, u32) -> u32) -> Option<Vec<u16>> {
    let mut buf = vec![0u16; 260];
    loop {
        let n = call(buf.as_mut_ptr(), buf.len() as u32) as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            buf.truncate(n);
            return Some(buf);
        }
        buf.resize(n + 1, 0);
    }
}

/// Whether the resolved program (NUL-terminated) is a `.bat` or `.cmd`
/// file, judged by its full path as std does.
fn is_batch_file(program: &[u16]) -> io::Result<bool> {
    use windows_sys::Win32::Storage::FileSystem::GetFullPathNameW;
    // SAFETY: `program` is NUL-terminated; the buffer and its length are
    // passed together.
    let full = fill(|buf, len| unsafe {
        GetFullPathNameW(program.as_ptr(), len, buf, std::ptr::null_mut())
    })
    .ok_or_else(io::Error::last_os_error)?;
    let tail = full.len().checked_sub(4).map(|i| &full[i..]);
    let lower = |unit: &u16| u8::try_from(*unit).map_or(0, |b| b.to_ascii_lowercase());
    Ok(tail.is_some_and(|tail| {
        let tail: Vec<u8> = tail.iter().map(lower).collect();
        tail == b".bat" || tail == b".cmd"
    }))
}

/// `cmd.exe` in the system directory, NUL-terminated.
fn command_prompt() -> io::Result<Vec<u16>> {
    let mut dir = system_directory().ok_or_else(io::Error::last_os_error)?;
    dir.push("cmd.exe");
    wide(dir.as_os_str())
}

/// The command line for `program args`, as `CommandLineToArgvW` (and the C
/// runtime) reads it back: the program in quotes, each argument quoted when
/// it is empty or holds a space or tab.
fn command_line(program: &OsStr, args: &[String]) -> io::Result<Vec<u16>> {
    let mut line = vec![u16::from(b'"')];
    line.extend(no_nuls(program)?.encode_wide());
    line.push(u16::from(b'"'));
    for arg in args {
        line.push(u16::from(b' '));
        let arg = no_nuls(OsStr::new(arg))?;
        let quote = arg.is_empty()
            || arg
                .as_encoded_bytes()
                .iter()
                .any(|&b| b == b' ' || b == b'\t');
        if quote {
            line.push(u16::from(b'"'));
        }
        let mut backslashes = 0;
        for unit in arg.encode_wide() {
            if unit == u16::from(b'\\') {
                backslashes += 1;
            } else {
                if unit == u16::from(b'"') {
                    // 2n+1 backslashes before an inner quote.
                    line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes + 1));
                }
                backslashes = 0;
            }
            line.push(unit);
        }
        if quote {
            // 2n backslashes before the closing quote.
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            line.push(u16::from(b'"'));
        }
    }
    Ok(line)
}

/// The `cmd.exe` command line that runs batch file `script`
/// (NUL-terminated) with `args`, quoted so `cmd` neither splits nor expands
/// them: std's rules after CVE-2024-24576. The script's own path cannot be
/// escaped inside cmd's quotes, so a path with `"`, `^` or `%`, or ending
/// in `\`, is refused, as std does since rust-lang/rust 2338216e.
fn batch_command_line(script: &[u16], args: &[String]) -> io::Result<Vec<u16>> {
    let script = script.strip_suffix(&[0]).unwrap_or(script);
    let special = b"\"^%".map(u16::from);
    if script.iter().any(|c| special.contains(c)) || script.last() == Some(&u16::from(b'\\')) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "batch scripts may not contain `\"`, `^` or `%` or end with `\\`",
        ));
    }
    let mut line: Vec<u16> = "cmd.exe /e:ON /v:OFF /d /c \"\"".encode_utf16().collect();
    line.extend_from_slice(script);
    line.push(u16::from(b'"'));
    for arg in args {
        line.push(u16::from(b' '));
        if arg.contains(['\r', '\n']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch file arguments are invalid",
            ));
        }
        append_batch_arg(&mut line, no_nuls(OsStr::new(arg))?);
    }
    line.push(u16::from(b'"'));
    Ok(line)
}

fn append_batch_arg(line: &mut Vec<u16>, arg: &OsStr) {
    const UNQUOTED: &str = r"#$*+-./:?@\_";
    let mut quote = arg.is_empty() || arg.as_encoded_bytes().last() == Some(&b'\\');
    for c in char::decode_utf16(arg.encode_wide()).flatten() {
        let ascii_needs_quotes =
            c.is_ascii() && !(c.is_ascii_alphanumeric() || UNQUOTED.contains(c));
        if ascii_needs_quotes || c.is_control() {
            quote = true;
        }
    }
    if quote {
        line.push(u16::from(b'"'));
    }
    let mut backslashes = 0;
    for unit in arg.encode_wide() {
        if unit == u16::from(b'\\') {
            backslashes += 1;
        } else {
            if unit == u16::from(b'"') {
                // 2n backslashes, then a doubled quote.
                line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
                line.push(u16::from(b'"'));
            } else if unit == u16::from(b'%') || unit == u16::from(b'\r') {
                // `%%cd:~,%` expands to nothing and keeps `%VAR%` unexpanded.
                line.extend("%%cd:~,".encode_utf16());
            }
            backslashes = 0;
        }
        line.push(unit);
    }
    if quote {
        line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
        line.push(u16::from(b'"'));
    }
}

/// The working directory, NUL-terminated, without a `\\?\` prefix, which
/// `CreateProcessW` does not accept (a canonicalised path has one).
fn current_directory(dir: &Path) -> io::Result<Vec<u16>> {
    let dir = wide(dir.as_os_str())?;
    let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
    let unc: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
    if dir.starts_with(&unc) {
        let mut plain: Vec<u16> = r"\\".encode_utf16().collect();
        plain.extend_from_slice(&dir[unc.len()..]);
        return Ok(plain);
    }
    if let Some(rest) = dir.strip_prefix(prefix.as_slice()) {
        // `X:\…`: a drive path the prefix only made verbatim.
        if rest.len() >= 3 && rest[1] == u16::from(b':') && rest[2] == u16::from(b'\\') {
            return Ok(rest.to_vec());
        }
    }
    Ok(dir)
}

fn no_nuls(s: &OsStr) -> io::Result<&OsStr> {
    if s.as_encoded_bytes().contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "nul byte found in provided data",
        ));
    }
    Ok(s)
}

/// `s` as a NUL-terminated wide string.
fn wide(s: &OsStr) -> io::Result<Vec<u16>> {
    Ok(no_nuls(s)?.encode_wide().chain([0]).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(program: &str, args: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        String::from_utf16(&command_line(OsStr::new(program), &args).unwrap()).unwrap()
    }

    /// Every argument comes back as given through `CommandLineToArgvW`.
    #[test]
    fn arguments_round_trip_through_the_command_line() {
        use windows_sys::Win32::UI::Shell::CommandLineToArgvW;
        let args = [
            "plain",
            "",
            "two words",
            "tab\there",
            r#"a"quote"#,
            r"trailing\",
            r"trailing space\ ",
            r#"back\\"slash"#,
            "ünïcödé",
        ];
        let owned: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let mut wide = command_line(OsStr::new(r"C:\x\prog.exe"), &owned).unwrap();
        wide.push(0);
        let mut argc = 0;
        // SAFETY: `wide` is NUL-terminated; the result is freed below.
        let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut argc) };
        assert!(!argv.is_null());
        let parsed: Vec<String> = (0..argc as usize)
            .map(|i| {
                // SAFETY: CommandLineToArgvW returned `argc` NUL-terminated strings.
                let p = unsafe { *argv.add(i) };
                let len = (0..).take_while(|&j| unsafe { *p.add(j) } != 0).count();
                String::from_utf16(unsafe { std::slice::from_raw_parts(p, len) }).unwrap()
            })
            .collect();
        // SAFETY: allocated by CommandLineToArgvW.
        unsafe { windows_sys::Win32::Foundation::LocalFree(argv.cast()) };
        assert_eq!(parsed[0], r"C:\x\prog.exe");
        assert_eq!(&parsed[1..], &args);
    }

    #[test]
    fn plain_arguments_are_not_quoted() {
        assert_eq!(line("ping", &["-n", "3"]), r#""ping" -n 3"#);
    }

    #[test]
    fn batch_arguments_are_quoted_against_cmd() {
        let args = vec!["a b".to_owned(), "%PATH%".to_owned(), "x&y".to_owned()];
        let script: Vec<u16> = r"C:\s\run.bat".encode_utf16().chain([0]).collect();
        let line = String::from_utf16(&batch_command_line(&script, &args).unwrap()).unwrap();
        assert_eq!(
            line,
            r#"cmd.exe /e:ON /v:OFF /d /c ""C:\s\run.bat" "a b" "%%cd:~,%PATH%%cd:~,%" "x&y"""#
        );
        assert!(batch_command_line(&script, &["a\nb".to_owned()]).is_err());
    }

    #[test]
    fn batch_script_paths_cmd_would_expand_are_refused() {
        for path in [
            r#"C:\s\"run.bat"#,
            r"C:\s^\run.bat",
            r"C:\%s%\run.bat",
            "C:\\s\\",
        ] {
            let script: Vec<u16> = path.encode_utf16().chain([0]).collect();
            let err = batch_command_line(&script, &[]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{path}");
        }
        let script: Vec<u16> = r"C:\s (x)\run.bat".encode_utf16().chain([0]).collect();
        assert!(batch_command_line(&script, &[]).is_ok());
    }

    #[test]
    fn a_verbatim_working_directory_loses_its_prefix() {
        let plain =
            |s: &str| String::from_utf16(&current_directory(Path::new(s)).unwrap()).unwrap();
        assert_eq!(plain(r"\\?\C:\work"), "C:\\work\0");
        assert_eq!(plain(r"\\?\UNC\server\share"), "\\\\server\\share\0");
        assert_eq!(plain(r"C:\work"), "C:\\work\0");
    }

    #[test]
    fn environment_names_compare_without_case() {
        let remove = BTreeSet::from(["path".to_owned()]);
        let set = BTreeMap::from([("Tendr_X".to_owned(), "1".to_owned())]);
        let mut env = Environment::inherited(&remove, &set);
        env.set("TENDR_X", "2".into());
        assert!(env.vars.iter().all(|(k, _)| fold(k) != "PATH"));
        let x: Vec<_> = env
            .vars
            .iter()
            .filter(|(k, _)| fold(k) == "TENDR_X")
            .collect();
        assert_eq!(x.len(), 1);
        assert_eq!(x[0].1, "2");
        // PATH was removed, so the child's PATH (none) is searched first.
        assert!(env.path_changed);
        assert_eq!(env.child_path(), None);
    }

    #[test]
    fn a_bare_name_is_found_on_the_system_path() {
        let found = resolve_program(OsStr::new("cmd"), None).unwrap();
        let found = String::from_utf16(&found[..found.len() - 1]).unwrap();
        assert!(found.to_lowercase().ends_with(r"\cmd.exe"), "{found}");
        assert_eq!(
            resolve_program(OsStr::new("no-such-program-tendr"), None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }
}
