use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::{env, fs};
use std::{ffi::CString, io::Read};
use std::{
    io::{BufRead, BufReader},
    str,
};
use std::{
    os::unix::fs::PermissionsExt,
    os::unix::io::{AsRawFd, FromRawFd},
    process::Command,
};
use std::{path::PathBuf, str::FromStr};

use anyhow::{anyhow, bail, Context, Result};
use nix::sys::{ptrace, wait};
use nix::unistd;
use structopt::clap::ArgMatches;
use structopt::StructOpt;
use strum::{EnumString, EnumVariantNames, VariantNames as _};

/// Launches the given command and attaches a debugger to it.
#[derive(Debug, StructOpt)]
#[structopt(name = "dbgee", about = "the active debuggee")]
struct Opts {
    #[structopt(subcommand)]
    pub command: Subcommand,

    /// Debugger to launch. Choose "auto", "gdb", "dlv", "stop-and-write-pid", "python"
    /// or you can specify an arbitrary command line.
    /// The debuggee's PID follows your command line as an argument.
    ///
    /// stop-and-write-pid: Stops the debuggee, and prints the debuggee's PID.
    /// dbgee writes the PID to /tmp/dbgee_pid. If stderr is a tty,
    /// dbgee outputs the PID to stderr as well.
    #[structopt(short, long, default_value("auto"))]
    pub debugger: String,
}

#[derive(Debug, StructOpt)]
enum Subcommand {
    Run(RunOpts),
    Set(SetOpts),
    Unset(UnsetOpts),
}

/// Launches the debuggee, and attaches the specified debugger to it.
#[derive(Debug, StructOpt)]
#[structopt(
    usage = "dbgee run [OPTIONS] -- <debuggee> [args-for-debuggee]...",
    rename_all = "kebab"
)]
struct RunOpts {
    /// Path to the debuggee process
    #[structopt()]
    pub debuggee: String,

    #[structopt(name = "args")]
    pub debuggee_args: Vec<String>,

    #[structopt(flatten)]
    attach_opts: AttachOpts,
}

const SETOPTS_POSITIONAL_ARGS: [&str; 2] = ["debuggee", "start-cmd"];

/// Replaces the debuggee with a wrapper script, so that the debugger will be attached to it whenever
/// it is launched by any processes from now on.
///
/// Please run "unset" command to restore the original debuggee if you don't want to attach debuggers anymore.
/// Or, if you give start_cmd, dbgee automatically does "unset" after start_cmd finishes.
#[derive(Debug, StructOpt)]
#[structopt(
    usage = "dbgee set [OPTIONS] <debuggee>  [-- <run_cmd> [args-for-debuggee]...]",
    rename_all = "kebab"
)]
struct SetOpts {
    /// Path to the debuggee process
    #[structopt()]
    pub debuggee: String,

    /// If start_cmd is given, dbgee launches start_cmd, and automatically unsets after
    /// start_cmd finishes
    #[structopt()]
    pub start_cmd: Vec<String>,

    #[structopt(flatten)]
    attach_opts: AttachOpts,
}

/// Removes the wrapper script which "set" put, and restores the original debuggee file.
#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab")]
struct UnsetOpts {
    /// Path to the debuggee process
    #[structopt(name = "debuggee")]
    pub debuggee: String,
}

#[derive(Debug, StructOpt)]
struct AttachOpts {
    /// Terminal to launch the debugger in.
    ///
    /// tmuxw (default): Opens a new tmux window in last active tmux session,
    /// launches a debugger there, and has the debugger attach to the debuggee.
    /// If there is no active tmux session, it launches a new session in the background,
    /// and writes a notification to stderr (as far as stderr is a tty).

    /// tmuxp: Opens a new tmux pane in last active tmux session.
    ///
    #[structopt(
        short,
        long,
        possible_values(DebuggerTerminalOpt::VARIANTS),
        default_value("tmuxw")
    )]
    pub terminal: DebuggerTerminalOpt,
}

#[derive(Debug, EnumString, EnumVariantNames)]
#[strum(serialize_all = "kebab-case")]
pub enum DebuggerTerminalOpt {
    Tmuxw,
    Tmuxp,
}

trait Debugger {
    fn run(&mut self, run_opts: &RunOpts, terminal: &dyn DebuggerTerminal) -> Result<i32>;
    fn set(&mut self, set_opts: &SetOpts, terminal: &dyn DebuggerTerminal) -> Result<()>;
    fn unset(&mut self, unset_opts: &UnsetOpts) -> Result<()>;
    fn build_attach_commandline(&self) -> Result<Vec<String>>;
}

trait DebuggerTerminal {
    fn open(&self, debugger: &dyn Debugger) -> Result<()>;
}

fn main() {
    if let Err(e) = run() {
        print_error(&e.to_string());
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let clap_matches = Opts::clap().get_matches();
    let opts = Opts::from_clap(&clap_matches);

    let debuggee = match opts.command {
        Subcommand::Run(ref run_opts) => &run_opts.debuggee,
        Subcommand::Set(ref set_opts) => &set_opts.debuggee,
        Subcommand::Unset(ref unset_opts) => &unset_opts.debuggee,
    };
    let mut debugger = build_debugger(&opts.debugger, debuggee)?;

    match opts.command {
        Subcommand::Run(run_opts) => {
            let debugger_terminal = build_debugger_terminal(&run_opts.attach_opts.terminal);
            let exit_status = debugger.run(&run_opts, debugger_terminal.as_ref())?;
            std::process::exit(exit_status);
        }
        Subcommand::Set(set_opts) => {
            let debugger_terminal = build_debugger_terminal(&set_opts.attach_opts.terminal);
            debugger.set(&set_opts, debugger_terminal.as_ref())?;
        }
        Subcommand::Unset(unset_opts) => {
            debugger.unset(&unset_opts)?;
        }
    };
    Ok(())
}

fn build_debugger(debugger: &str, debuggee: &str) -> Result<Box<dyn Debugger>> {
    match debugger {
        "auto" => detect_debugger(debuggee),
        "gdb" => Ok(Box::new(GdbDebugger::new()?)),
        "dlv" => Ok(Box::new(DelveDebugger::new()?)),
        "stop-and-write-pid" => Ok(Box::new(StopAndWritePidDebugger::new())),
        "debugpy" => Ok(Box::new(PythonDebugger::new()?)),
        _ => Err(anyhow!("Unsupported debugger: {}", debugger)),
    }
}

fn detect_debugger(debuggee: &str) -> Result<Box<dyn Debugger>> {
    let file_output = Command::new("file").args(&[debuggee]).output()?;
    let file_output = str::from_utf8(&file_output.stdout)?;
    if file_output.contains("Go ") {
        return Ok(Box::new(DelveDebugger::new()?));
    }
    if file_output.contains("ELF") {
        return Ok(Box::new(GdbDebugger::new()?));
    }
    if file_output.contains("Python") {
        return Ok(Box::new(PythonDebugger::new()?));
    }
    bail!("Could not detect the language of the debuggee")
}

fn build_debugger_terminal(action: &DebuggerTerminalOpt) -> Box<dyn DebuggerTerminal> {
    match action {
        DebuggerTerminalOpt::Tmuxw => Box::new(Tmux::new(TmuxLayout::NewWindow)),
        DebuggerTerminalOpt::Tmuxp => Box::new(Tmux::new(TmuxLayout::NewPane)),
    }
}

trait PidAttachableBinaryDebugger {
    fn build_attach_commandline(&self) -> Result<Vec<String>>;
    fn set_debuggee_pid(&mut self, pid: unistd::Pid);
}

impl<T: PidAttachableBinaryDebugger> Debugger for T {
    fn run(&mut self, run_opts: &RunOpts, terminal: &dyn DebuggerTerminal) -> Result<i32> {
        let debuggee_cmd: Vec<&String> = vec![&run_opts.debuggee]
            .into_iter()
            .chain(run_opts.debuggee_args.iter())
            .collect();
        let debuggee_pid = fork_exec_stop(&debuggee_cmd)?;
        self.set_debuggee_pid(debuggee_pid);
        terminal.open(self)?;
        wait_until_exit(debuggee_pid)
    }

    fn set(&mut self, set_opts: &SetOpts, terminal: &dyn DebuggerTerminal) -> Result<()> {
        set_to_exec_dgeee(set_opts, terminal)
    }

    fn unset(&mut self, unset_opts: &UnsetOpts) -> Result<()> {
        unset_from_exec_dbgee(unset_opts)
    }

    fn build_attach_commandline(&self) -> Result<Vec<String>> {
        self.build_attach_commandline()
    }
}

struct GdbDebugger {
    debuggee_pid: unistd::Pid,
}

impl GdbDebugger {
    fn new() -> Result<GdbDebugger> {
        if !command_exists("gdb") {
            bail!("'gdb' is not in PATH. Did you install gdb?")
        }
        Ok(GdbDebugger {
            debuggee_pid: unistd::Pid::this(),
        })
    }
}

impl PidAttachableBinaryDebugger for GdbDebugger {
    fn set_debuggee_pid(&mut self, pid: unistd::Pid) {
        self.debuggee_pid = pid;
    }

    fn build_attach_commandline(&self) -> Result<Vec<String>> {
        Ok(vec![
            "gdb".to_owned(),
            "-p".to_owned(),
            self.debuggee_pid.as_raw().to_string(),
        ])
    }
}

struct DelveDebugger {
    debuggee_pid: unistd::Pid,
}

impl DelveDebugger {
    fn new() -> Result<DelveDebugger> {
        if !command_exists("dlv") {
            bail!("'dlv' is not in PATH. Did you install delve?")
        }
        Ok(DelveDebugger {
            debuggee_pid: unistd::Pid::this(),
        })
    }
}

impl PidAttachableBinaryDebugger for DelveDebugger {
    fn set_debuggee_pid(&mut self, pid: unistd::Pid) {
        self.debuggee_pid = pid;
    }

    fn build_attach_commandline(&self) -> Result<Vec<String>> {
        Ok(vec![
            "dlv".to_owned(),
            "attach".to_owned(),
            self.debuggee_pid.as_raw().to_string(),
        ])
    }
}

struct StopAndWritePidDebugger;

impl StopAndWritePidDebugger {
    fn new() -> StopAndWritePidDebugger {
        StopAndWritePidDebugger {}
    }
}

impl Debugger for StopAndWritePidDebugger {
    fn run(&mut self, run_opts: &RunOpts, _terminal: &dyn DebuggerTerminal) -> Result<i32> {
        let debuggee_cmd: Vec<&String> = vec![&run_opts.debuggee]
            .into_iter()
            .chain(run_opts.debuggee_args.iter())
            .collect();
        let debuggee_pid = fork_exec_stop(&debuggee_cmd)?;
        print_message("The debuggee process is paused. Atach a debugger to it by PID.");
        print_message(&format!(
            "PID: {}. It's also written to /tmp/dbgee_pid as a plain text number.",
            debuggee_pid.as_raw()
        ));
        print_message("This message is suppressed if the stderr is redirected or piped.");
        let mut pid_file = File::create("/tmp/dbgee_pid")?;
        write!(pid_file, "{}", debuggee_pid.as_raw())?;
        wait_until_exit(debuggee_pid)
    }

    fn set(&mut self, set_opts: &SetOpts, terminal: &dyn DebuggerTerminal) -> Result<()> {
        set_to_exec_dgeee(set_opts, terminal)
    }

    fn unset(&mut self, unset_opts: &UnsetOpts) -> Result<()> {
        unset_from_exec_dbgee(unset_opts)
    }

    fn build_attach_commandline(&self) -> Result<Vec<String>> {
        bail!("build_attach_commandline should not be called for StopAndWritePidDebugger");
    }
}

struct PythonDebugger {
    python_command: String,
}

impl PythonDebugger {
    fn new() -> Result<PythonDebugger> {
        let python_path;
        if command_exists("python3") {
            python_path = "python3".to_owned();
        } else if command_exists("python") {
            python_path = "python".to_owned();
        } else {
            bail!("Neither 'python3' nor 'python' exist. Did you install python?");
        }

        let debugpy_exists = Command::new(&python_path)
            .args(&["-c", "'import debugpy'"])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status();
        if debugpy_exists.is_err() || !debugpy_exists.unwrap().success() {
            bail!("'debugpy' module is not installed. Please install debugpy via pip.");
        }

        Ok(PythonDebugger {
            python_command: python_path,
        })
    }
}

impl Debugger for PythonDebugger {
    fn run(&mut self, run_opts: &RunOpts, _terminal: &dyn DebuggerTerminal) -> Result<i32> {
        print_message(
            "The debuggee process is paused. Attach to it in VSCode. \
                 VSCode is the only supported debugger for Python.",
        );
        print_message("Port: 5679");
        print_message("This message is suppressed if the stderr is redirected or piped.");
        let debuggee_args: Vec<&str> = vec![
            "-m",
            "debugpy",
            "--wait-for-client",
            "--listen",
            "5679",
            &run_opts.debuggee,
        ]
        .into_iter()
        .chain(run_opts.debuggee_args.iter().map(|s| s.as_str()))
        .collect();
        let exit_status = Command::new(&self.python_command)
            .args(&debuggee_args)
            .status()
            .with_context(|| "failed to launch debugpy. Perhaps is port 5679 being used?")?;
        exit_status
            .code()
            .ok_or_else(|| anyhow!("ExitStatus.code() failed for an unexpected reason"))
    }

    fn set(&mut self, _set_opts: &SetOpts, _terminal: &dyn DebuggerTerminal) -> Result<()> {
        bail!("set is not implemented yet for Python");
    }

    fn unset(&mut self, _unset_opts: &UnsetOpts) -> Result<()> {
        bail!("unset is not implemented yet for Python");
    }

    fn build_attach_commandline(&self) -> Result<Vec<String>> {
        bail!("build_attach_commandline should not be called for PythonDebugger");
    }
}

struct Tmux {
    layout: TmuxLayout,
}

enum TmuxLayout {
    NewWindow,
    NewPane,
}

impl Tmux {
    fn new(layout: TmuxLayout) -> Tmux {
        Tmux { layout }
    }
}

impl TmuxLayout {
    fn to_command(&self) -> Vec<&str> {
        match self {
            TmuxLayout::NewWindow => vec!["new-window"],
            TmuxLayout::NewPane => vec!["splitw", "-h"],
        }
    }
}

impl DebuggerTerminal for Tmux {
    fn open(&self, debugger: &dyn Debugger) -> Result<()> {
        let debugger_cmd = debugger.build_attach_commandline()?;
        let is_tmux_active = Command::new("tmux")
            .args(&["ls"])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .with_context(|| "Failed to launch tmux. Is tmux installed?")?;

        if is_tmux_active.success() {
            let mut args = self.layout.to_command();
            args.extend(debugger_cmd.iter().map(|s| s.as_str()));
            Command::new("tmux")
                .args(&args)
                .status()
                .with_context(|| "Failed to open a new tmux window for an unexpected reason.")?;
        } else {
            let mut args = vec!["new-session"];
            args.extend(debugger_cmd.iter().map(|s| s.as_str()));
            Command::new("tmux")
                .args(&args)
                .spawn()
                .with_context(|| "Failed to open a new tmux session for an unexpected reason.")?;
            print_message(
                "the debugger has launched in a new tmux session. Try `tmux a` to attach.",
            );
        }

        Ok(())
    }
}

fn set_to_exec_dgeee(set_opts: &SetOpts, _terminal: &dyn DebuggerTerminal) -> Result<()> {
    let clap_matches = Opts::clap().get_matches();
    let run_command = build_run_command(&clap_matches)?;
    wrap_debuggee_binary(&set_opts.debuggee, &run_command)?;

    if set_opts.start_cmd.is_empty() {
        return Ok(());
    }

    let mut child = Command::new(&set_opts.start_cmd[0])
        .args(&set_opts.start_cmd[1..])
        .spawn()?;
    let _ = child.wait()?;

    unwrap_debuggee_binary(&set_opts.debuggee)
}

fn unset_from_exec_dbgee(unset_opts: &UnsetOpts) -> Result<()> {
    unwrap_debuggee_binary(&unset_opts.debuggee)
}

fn wrap_debuggee_binary(debuggee: &str, run_command: &str) -> Result<()> {
    if check_if_wrapped(debuggee) {
        bail!(
            "{} is already wrapped by dbgee. Did you set it already?",
            debuggee
        );
    }

    let debuggee_path = get_valid_executable_path(Path::new(debuggee), "the debuggee")?;

    let wrapper_sh_template_bytes = include_bytes!("../resources/wrapper.sh");
    let wrapper_sh_template = str::from_utf8(wrapper_sh_template_bytes).unwrap();
    let wrapper_sh = wrapper_sh_template
        .replace("%run_cmd%", run_command)
        .replace("%debuggee%", &format!("\"{}\"", &debuggee_path));

    let mut debuggee_pathbuf = PathBuf::from_str(&debuggee_path)?;
    // unwrap should be OK here because debuggee_path is a valid UTF-8 path of an executable file.
    let debuggee_filename = debuggee_pathbuf
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    debuggee_pathbuf.pop();
    debuggee_pathbuf.push(get_debuggee_backup_name(&debuggee_filename));
    let debuggee_backup = debuggee_pathbuf.to_str().unwrap();

    let debuggee_perm = fs::metadata(&debuggee_path)?.permissions();
    fs::rename(&debuggee_path, &debuggee_backup)?;
    fs::write(&debuggee_path, wrapper_sh)?;
    fs::set_permissions(&debuggee_path, debuggee_perm)?;

    Ok(())
}

fn unwrap_debuggee_binary(debuggee: &str) -> Result<()> {
    let wrapper_path = get_valid_executable_path(Path::new(debuggee), "the debuggee")?;

    if !check_if_wrapped(debuggee) {
        bail!(
            "{} is not wrapped by dbgee. Did you unset it already?",
            debuggee
        );
    }

    let mut wrapper_pathbuf = PathBuf::from_str(&wrapper_path)?;
    // unwrap should be OK here because wrapper_path is a valid UTF-8 path of an executable file.
    let debuggee_filename = wrapper_pathbuf
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    wrapper_pathbuf.pop();
    wrapper_pathbuf.push(get_debuggee_backup_name(&debuggee_filename));
    let debuggee_path = wrapper_pathbuf.to_str().unwrap();

    fs::remove_file(&wrapper_path)?;
    fs::rename(&debuggee_path, &wrapper_path)?;

    Ok(())
}

fn check_if_wrapped<P: AsRef<Path>>(path: P) -> bool {
    let wrapper_sh_template_bytes = include_bytes!("../resources/wrapper.sh");
    let wrapper_sh_template = str::from_utf8(wrapper_sh_template_bytes).unwrap();
    let wrapper_signature: String = wrapper_sh_template.lines().take(2).collect();

    let file = File::open(path);
    if file.is_err() {
        return false;
    }
    let file = file.unwrap();

    let lines = BufReader::new(file).lines();
    let signature: String = lines
        .take(2)
        .map(|lresult| lresult.unwrap_or_default())
        .collect();

    signature == wrapper_signature
}

fn build_run_command(set_opts: &ArgMatches) -> Result<String> {
    let self_pathbuf = env::current_exe()?;
    let self_path = get_valid_executable_path(&self_pathbuf, "dbgee")?;
    let global_opts = reconstruct_flags(set_opts, &[]);
    let attach_opts = reconstruct_flags(
        set_opts.subcommand_matches("set").unwrap(),
        &SETOPTS_POSITIONAL_ARGS,
    );
    let debuggee_path = get_valid_executable_path(
        &set_opts
            .subcommand_matches("set")
            .unwrap()
            .value_of("debuggee")
            .unwrap(),
        "debuggee",
    )?;

    Ok(format!(
        "{} {} run {} -- {} \"$@\"",
        self_path,
        &global_opts,
        &attach_opts,
        &get_debuggee_backup_name(&debuggee_path)
    ))
}

fn get_debuggee_backup_name(debuggee_filename: &str) -> String {
    format!("{}-original", debuggee_filename)
}

fn reconstruct_flags(opts: &ArgMatches, positional_args: &[&str]) -> String {
    let mut command = vec![];
    for &key in opts.args.keys() {
        if positional_args.contains(&key) {
            continue;
        }
        command.push(format!("--{}", key.replace("_", "-")));
        if let Some(values) = opts.values_of(key) {
            for value in values {
                command.push(format!("'{}'", escape_single_quote(value)));
            }
        }
    }
    command.join(" ")
}

fn escape_single_quote(s: &str) -> String {
    s.replace("'", "'\"'\"'")
}

fn command_exists(command: &str) -> bool {
    if let Ok(path) = env::var("PATH") {
        for dir in path.split(':') {
            let path = format!("{}/{}", dir, command);
            if is_executable(path) {
                return true;
            }
        }
    }
    false
}

fn get_valid_executable_path<T: AsRef<Path>>(path: T, name: &str) -> Result<String> {
    let abspath = get_abspath(path, name)?;
    if !is_executable(&abspath) {
        bail!("{} ({}) is not executable", name, abspath);
    }
    Ok(abspath)
}

fn get_abspath<T: AsRef<Path>>(path: T, name: &str) -> Result<String> {
    path.as_ref()
        .to_str()
        .ok_or_else(|| anyhow!("{} should exist in a valid UTF-8 path", name))?;
    let abspath = fs::canonicalize(&path).with_context(|| {
        format!(
            "{} (Path: '{}') does not exist.",
            &name,
            &path.as_ref().to_str().unwrap()
        )
    })?;
    let abspath = abspath.as_os_str().to_str().ok_or_else(|| {
        anyhow!(
            "{}",
            message_string(format!(
                "Error: {} should exist in a valid UTF-8 path",
                name
            ))
        )
    })?;
    Ok(abspath.to_owned())
}

fn is_executable<P: AsRef<Path>>(path: P) -> bool {
    if let Ok(metadata) = fs::metadata(path) {
        // TODO: more fine-grained permission check
        if metadata.is_file() && (metadata.permissions().mode() & 0o111 != 0) {
            return true;
        }
    }
    false
}

fn fork_exec_stop<T: AsRef<str>>(debuggee_cmd: &[T]) -> Result<unistd::Pid> {
    get_valid_executable_path(debuggee_cmd[0].as_ref(), "the debuggee")?;
    let (read_fd, write_fd) =
        unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).with_context(|| "pipe2 failed")?;
    let mut sync_pipe_read: File = unsafe { File::from_raw_fd(read_fd) };
    let mut sync_pipe_write: File = unsafe { File::from_raw_fd(write_fd) };
    match unsafe { unistd::fork().with_context(|| "fork failed.")? } {
        unistd::ForkResult::Child => {
            let mut buf = [0; 1];
            let _ = sync_pipe_read.read(&mut buf);
            let cargs: Vec<CString> = debuggee_cmd
                .iter()
                .map(|arg| CString::new(arg.as_ref()).unwrap())
                .collect();
            let _ = unistd::execv(&cargs[0], &cargs[0..]);
            bail!(
                "exec {} failed. Error: {}",
                &cargs[0].to_str().unwrap(),
                nix::Error::last()
            );
        }
        unistd::ForkResult::Parent {
            child: debuggee_pid,
        } => {
            ptrace::attach(debuggee_pid).with_context(|| {
                "ptrace attach failed. Perhaps dgbee is being traced by some debugger?"
            })?;
            let buf = [0; 1];
            let _ = sync_pipe_write.write(&buf);
            // Wait for the debuggee to be stopped by SIGSTOP, which is triggered by PTRACE_ATTACH
            match wait::waitpid(debuggee_pid, None)
                .with_context(|| "Unexpected error. Waiting for SIGSTOP failed.")?
            {
                wait::WaitStatus::Stopped(_, nix::sys::signal::SIGSTOP) => {}
                other => {
                    eprintln!(
                        "The observed signal is not SISTOP, but dbgee continues. {:?}",
                        other
                    );
                }
            }

            ptrace::cont(debuggee_pid, None)
                .with_context(|| "Unexpected error. Continuing the process failed")?;
            match wait::waitpid(debuggee_pid, None)
                .with_context(|| "Unexpected error. Waiting for SIGTRAP failed.")?
            {
                wait::WaitStatus::Exited(_, _) => {
                    panic!("The process exited for an unexpected reason");
                }
                wait::WaitStatus::Stopped(_, nix::sys::signal::SIGTRAP) => {}
                other => {
                    eprintln!(
                        "The observed signal is not SIGTRAP, but continues. {:?}",
                        other
                    );
                }
            }

            ptrace::detach(debuggee_pid, nix::sys::signal::SIGSTOP)
                .with_context(|| "Unexpected error. Detach and stop failed")?;

            Ok(debuggee_pid)
        }
    }
}

fn wait_until_exit(pid: unistd::Pid) -> Result<i32> {
    loop {
        if let wait::WaitStatus::Exited(_, exit_status) = wait::waitpid(pid, None)? {
            return Ok(exit_status);
        }
    }
}

fn print_error<T: AsRef<str>>(mes: T) {
    eprintln!("{}", message_string(format!("Error: {}", mes.as_ref())));
}

fn print_message<T: AsRef<str>>(mes: T) {
    if let Ok(true) = unistd::isatty(std::io::stderr().as_raw_fd()) {
        eprintln!("{}", message_string(mes.as_ref()))
    }
}

fn message_string<T: AsRef<str>>(mes: T) -> String {
    format!("[Dbgee] {}", mes.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use tempfile::NamedTempFile;

    #[test]
    fn test_check_if_wrapperd_by_strings() {
        let actually_wrapped = indoc! {r#"
            #!/bin/sh
            # a wrapper script generated by dbgee
            some scripts
        "#};
        let tmpfile = make_temp_file(actually_wrapped);
        assert!(check_if_wrapped(tmpfile.path()));

        let not_wrapped = indoc! {r#"
            #!/bin/sh
            some scripts in the wild
        "#};
        let tmpfile = make_temp_file(not_wrapped);
        assert!(!check_if_wrapped(tmpfile.path()));
    }

    #[test]
    fn test_check_if_wrapperd_by_actually_wrapping() {
        let tmpfile = make_temp_file("dummy");
        let tmpfile_path = tmpfile.path().to_str().unwrap();
        wrap_debuggee_binary(tmpfile_path, "dummy run -- debuggee").unwrap();
        assert!(check_if_wrapped(tmpfile.path()));
        unwrap_debuggee_binary(tmpfile_path).unwrap();
        assert!(!check_if_wrapped(tmpfile.path()));
    }

    #[test]
    fn test_double_wrapping() {
        let tmpfile = make_temp_file("dummy");
        let tmpfile_path = tmpfile.path().to_str().unwrap();
        wrap_debuggee_binary(tmpfile_path, "dummy run -- debuggee").unwrap();
        assert!(wrap_debuggee_binary(tmpfile_path, "dummy run -- debuggee").is_err());
    }

    #[test]
    fn test_double_unwrapping() {
        let tmpfile = make_temp_file("dummy");
        let tmpfile_path = tmpfile.path().to_str().unwrap();
        wrap_debuggee_binary(tmpfile_path, "dummy run -- debuggee").unwrap();
        unwrap_debuggee_binary(tmpfile_path).unwrap();
        assert!(unwrap_debuggee_binary(tmpfile_path).is_err());
    }

    #[test]
    fn test_build_run_command_normal() {
        let debuggee_file = make_temp_executable_file("dummy");
        let debuggee = debuggee_file.as_ref().to_str().unwrap();
        let start_cmd_file = make_temp_executable_file("dummy");
        let start_cmd = start_cmd_file.as_ref().to_str().unwrap();
        let current_exe_pathbuf = std::env::current_exe().unwrap();
        let current_exe = current_exe_pathbuf.to_str().unwrap();

        let command = vec![
            current_exe,
            "set",
            debuggee,
            "-t",
            "tmuxw",
            "--",
            start_cmd,
            "some_args",
        ];
        let clap_matches = Opts::clap().get_matches_from(command.iter());

        let constructed_run_command: Vec<String> = build_run_command(&clap_matches)
            .unwrap()
            .split(' ')
            .map(|s| strip_quote(s).to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        let constructed_clap_matches =
            Opts::clap().get_matches_from(constructed_run_command.iter());

        let expected = vec![current_exe, "run", "-t", "tmuxw", "--", debuggee];
        let expected_clap_matches = Opts::clap().get_matches_from(expected.iter());

        assert!(compare_argmatches(
            &expected_clap_matches,
            &constructed_clap_matches
        ));
    }

    fn strip_quote(s: &str) -> &str {
        if s.starts_with('\'') {
            &s[1..s.len() - 1]
        } else {
            s
        }
    }

    fn compare_argmatches(a: &ArgMatches, b: &ArgMatches) -> bool {
        a.args.len() == b.args.len()
            && a.args
                .keys()
                .all(|a_key| a.args.get(*a_key).unwrap().vals == b.args.get(*a_key).unwrap().vals)
    }

    fn make_temp_executable_file(contents: &str) -> NamedTempFile {
        let tempfile = make_temp_file(contents);
        fs::set_permissions(tempfile.as_ref(), fs::Permissions::from_mode(0o555)).unwrap();
        tempfile
    }

    fn make_temp_file(contents: &str) -> NamedTempFile {
        let mut tempfile = NamedTempFile::new().unwrap();
        tempfile.write_all(contents.as_bytes()).unwrap();
        tempfile
    }
}
