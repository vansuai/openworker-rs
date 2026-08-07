//! Persistent shell executor for OpenWorker agents.
//!
//! `LocalExecutor` keeps one long-lived shell process (`/bin/bash` on POSIX,
//! `powershell.exe` on Windows), so `cd`, `export`, activated venvs, etc. persist
//! across `run_shell` calls. Each command appends a marker line (`__COWORKER_DONE_<uuid>__
//! <exit_code> <cwd>`) so the reader can synchronise and extract the exit code.
//!
//! Background tasks (`run_shell` with `run_in_background`) get their own detached
//! process — NOT the persistent shell — so dev servers run while the session keeps
//! working.
//!
//! Safety: permission-gating (high-risk tool → approval in the engine) + per-command
//! timeout + best-effort non-interactive enforcement.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command as StdCommand;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ocw_engine::{ToolFn, ToolRegistry, ToolResult, ToolSchema, ToolSpec};
use serde_json::{json, Map, Value};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_TIMEOUT_SECS: f64 = 120.0;
const MAX_TIMEOUT_SECS: f64 = 600.0;
const MAX_OUTPUT_CHARS: usize = 20_000;

// ---------------------------------------------------------------------------
// Background task
// ---------------------------------------------------------------------------

struct BackgroundTask {
    #[allow(dead_code)]
    task_id: String,
    #[allow(dead_code)]
    command: String,
    process: Child,
    #[allow(dead_code)]
    reader_thread: thread::JoinHandle<()>,
    lines: Arc<Mutex<Vec<String>>>,
    cursor: Arc<Mutex<usize>>,
}

impl BackgroundTask {
    fn new(
        task_id: String,
        command: &str,
        cwd: &Path,
        envs: &HashMap<String, String>,
    ) -> Result<Self, String> {
        let (shell, shell_arg) = if cfg!(windows) {
            ("powershell.exe", "-Command")
        } else {
            ("/bin/bash", "-c")
        };

        let mut cmd = Command::new(shell);
        cmd.arg(shell_arg)
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(cwd);

        for (k, v) in envs {
            cmd.env(k, v);
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP
        }
        #[cfg(unix)]
        {
            unsafe {
                cmd.pre_exec(|| Ok(()));
            }
        }

        let mut process = cmd
            .spawn()
            .map_err(|e| format!("failed to start background task: {e}"))?;
        let stdout = process.stdout.take();
        let stderr = process.stderr.take();

        let lines = Arc::new(Mutex::new(Vec::new()));
        let cursor = Arc::new(Mutex::new(0usize));
        let lines_clone = Arc::clone(&lines);

        let reader_thread = thread::spawn(move || {
            let read_lines = |reader: Box<dyn std::io::Read + Send>| {
                let buf = BufReader::new(reader);
                for l in buf.lines().map_while(Result::ok) {
                    lines_clone.lock().unwrap().push(l);
                }
            };
            if let Some(out) = stdout {
                read_lines(Box::new(out));
            }
            if let Some(err) = stderr {
                read_lines(Box::new(err));
            }
        });

        Ok(Self {
            task_id,
            command: command.to_string(),
            process,
            reader_thread,
            lines,
            cursor,
        })
    }

    fn read_new(&self) -> String {
        let lines = self.lines.lock().unwrap();
        let mut cursor = self.cursor.lock().unwrap();
        let new: String = lines[*cursor..].join("\n");
        *cursor = lines.len();
        new
    }

    fn exit_code(&mut self) -> Option<i32> {
        self.process
            .try_wait()
            .ok()
            .flatten()
            .and_then(|s| s.code())
    }

    fn kill(&mut self) {
        if self.process.try_wait().ok().flatten().is_some() {
            return;
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &self.process.id().to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        #[cfg(unix)]
        {
            let pid = self.process.id() as i32;
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
        }
    }
}

impl Drop for BackgroundTask {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

// ---------------------------------------------------------------------------
// LocalExecutor — persistent shell
// ---------------------------------------------------------------------------

pub struct LocalExecutor {
    cwd: Mutex<PathBuf>,
    shell_path: String,
    envs: HashMap<String, String>,
    default_timeout: Duration,
    max_output_chars: usize,
    marker: String,
    is_windows: bool,
    bg_tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
    bg_counter: Mutex<u64>,
    // Shell process management
    process: Mutex<Option<ProcessHandle>>,
    abort: Arc<AtomicBool>,
}

struct ProcessHandle {
    child: Child,
    #[allow(dead_code)]
    reader_thread: thread::JoinHandle<()>,
    lines: Arc<Mutex<Vec<String>>>,
    eof: Arc<AtomicBool>,
}

impl ProcessHandle {
    fn spawn(
        shell_path: &str,
        cwd: &Path,
        envs: &HashMap<String, String>,
        is_windows: bool,
    ) -> Result<Self, String> {
        let mut cmd = if is_windows {
            let mut c = Command::new(shell_path);
            c.args([
                "-NoProfile",
                "-NoLogo",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "-",
            ]);
            c
        } else {
            Command::new(shell_path)
        };

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(cwd);

        for (k, v) in envs {
            cmd.env(k, v);
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP
        }
        #[cfg(unix)]
        {
            unsafe {
                cmd.pre_exec(|| Ok(()));
            }
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to start shell: {e}"))?;

        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;
        let lines = Arc::new(Mutex::new(Vec::new()));
        let eof = Arc::new(AtomicBool::new(false));
        let lines_clone = Arc::clone(&lines);
        let eof_clone = Arc::clone(&eof);

        let reader_thread = thread::spawn(move || {
            let combined = stdout.chain(stderr);
            let buf = BufReader::new(combined);
            for line in buf.lines() {
                match line {
                    Ok(l) => lines_clone.lock().unwrap().push(l),
                    Err(_) => break,
                }
            }
            eof_clone.store(true, Ordering::SeqCst);
        });

        // PowerShell: silence the REPL prompt
        if is_windows {
            if let Some(mut stdin) = child.stdin.as_ref() {
                let _ = writeln!(stdin, "function prompt {{ '' }}");
                let _ = stdin.flush();
            }
        }

        Ok(Self {
            child,
            reader_thread,
            lines,
            eof,
        })
    }

    fn poll(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    fn eof(&self) -> bool {
        self.eof.load(Ordering::SeqCst)
    }

    fn drain_lines(&self) -> Vec<String> {
        let mut ls = self.lines.lock().unwrap();
        let out = ls.clone();
        ls.clear();
        out
    }

    fn stdin(&mut self) -> Option<&mut dyn Write> {
        self.child.stdin.as_mut().map(|s| s as &mut dyn Write)
    }

    fn kill_tree(&mut self) {
        let pid = self.child.id();
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        #[cfg(unix)]
        {
            let _ = self.child.kill();
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

impl LocalExecutor {
    pub fn new(
        cwd: &Path,
        env: Option<HashMap<String, String>>,
        shell_path: Option<&str>,
        default_timeout: Option<Duration>,
        max_output_chars: Option<usize>,
    ) -> Result<Self, String> {
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let is_windows = cfg!(windows);
        let shell_path = shell_path
            .unwrap_or(if is_windows {
                "powershell.exe"
            } else {
                "/bin/bash"
            })
            .to_string();
        let shell_path = if shell_path.is_empty() {
            if is_windows {
                "powershell.exe".to_string()
            } else {
                "/bin/bash".to_string()
            }
        } else {
            shell_path
        };

        let mut envs: HashMap<String, String> = std::env::vars().collect();
        // Non-interactive defaults
        envs.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
        envs.insert("DEBIAN_FRONTEND".into(), "noninteractive".into());
        envs.insert("PYTHONUNBUFFERED".into(), "1".into());
        envs.insert("PIP_NO_INPUT".into(), "1".into());
        if let Some(user_env) = env {
            for (k, v) in user_env {
                envs.insert(k, v);
            }
        }

        let marker = format!("__COWORKER_DONE_{}__", Uuid::new_v4().simple());
        let max_output_chars = max_output_chars.unwrap_or(MAX_OUTPUT_CHARS);
        let default_timeout =
            default_timeout.unwrap_or(Duration::from_secs_f64(DEFAULT_TIMEOUT_SECS));

        let process = ProcessHandle::spawn(&shell_path, &cwd, &envs, is_windows)?;

        Ok(Self {
            cwd: Mutex::new(cwd),
            shell_path,
            envs,
            default_timeout,
            max_output_chars,
            marker,
            is_windows,
            bg_tasks: Arc::new(Mutex::new(HashMap::new())),
            bg_counter: Mutex::new(0),
            process: Mutex::new(Some(process)),
            abort: Arc::new(AtomicBool::new(false)),
        })
    }

    fn ensure_shell(&self) {
        let mut guard = self.process.lock().unwrap();
        if let Some(ref mut proc) = *guard {
            if proc.poll().is_some() || proc.eof() {
                // Shell died — respawn
                match ProcessHandle::spawn(
                    &self.shell_path,
                    &self.cwd.lock().unwrap(),
                    &self.envs,
                    self.is_windows,
                ) {
                    Ok(new_proc) => *guard = Some(new_proc),
                    Err(_) => *guard = None,
                }
            }
        }
    }

    /// Run a foreground command in the persistent shell.
    /// Returns a JSON value with command, exit_code, cwd, output, timed_out, truncated.
    pub fn run(&self, command: &str, timeout: Option<Duration>) -> Value {
        self.ensure_shell();
        let timeout = timeout.unwrap_or(self.default_timeout);
        let timeout = timeout.min(Duration::from_secs_f64(MAX_TIMEOUT_SECS));
        self.abort.store(false, Ordering::SeqCst);

        let mut guard = self.process.lock().unwrap();
        let proc = match guard.as_mut() {
            Some(p) => p,
            None => {
                return json!({"command": command, "error": "shell not running", "cwd": self.cwd.lock().unwrap().display().to_string(), "exit_code": null, "output": "", "timed_out": false})
            }
        };

        let stdin = match proc.stdin() {
            Some(s) => s,
            None => {
                return json!({"command": command, "error": "shell not running", "cwd": self.cwd.lock().unwrap().display().to_string(), "exit_code": null, "output": "", "timed_out": false})
            }
        };

        // Write command + trailer
        let trailer = self.trailer();
        let _ = writeln!(stdin, "{command}");
        let _ = write!(stdin, "{trailer}");
        let _ = stdin.flush();

        let deadline = Instant::now() + timeout;
        let mut interrupted = false;
        let mut timed_out = false;
        let mut aborted = false;
        let mut exit_code: Option<i32> = None;
        let mut collected: Vec<String> = Vec::new();

        // Wait for marker
        loop {
            if self.abort.load(Ordering::SeqCst) {
                aborted = true;
                // Use existing deadline for interrupt path
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if self.is_windows {
                    timed_out = true;
                    proc.kill_tree();
                    // Wait for reaping
                    let _ = proc.child.wait();
                    break;
                }
                if !interrupted {
                    #[allow(unused_assignments)]
                    {
                        interrupted = true;
                    }
                    timed_out = true;
                    self.interrupt_inner(proc);
                    // Brief grace to resync
                    let grace = Instant::now() + Duration::from_secs(3);
                    // Read any remaining lines
                    loop {
                        if Instant::now() >= grace {
                            break;
                        }
                        let new_lines = proc.drain_lines();
                        if new_lines.is_empty() {
                            thread::sleep(Duration::from_millis(50));
                            continue;
                        }
                        for line in new_lines {
                            if self.parse_marker(&line, &mut exit_code) {
                                break;
                            }
                            collected.push(line);
                        }
                        if exit_code.is_some() {
                            break;
                        }
                    }
                    if exit_code.is_some() {
                        break;
                    }
                    // Still no marker — hard kill
                    proc.kill_tree();
                    let _ = proc.child.wait();
                    break;
                }
            }

            let new_lines = proc.drain_lines();
            if new_lines.is_empty() {
                if proc.eof() && proc.poll().is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(30));
                continue;
            }

            for line in new_lines {
                if self.parse_marker(&line, &mut exit_code) {
                    break;
                }
                collected.push(line);
            }
            if exit_code.is_some() {
                break;
            }
        }

        drop(guard);

        let output = collected.join("\n");
        let truncated = output.len() > self.max_output_chars;
        let output = if truncated {
            output[output.len().saturating_sub(self.max_output_chars)..].to_string()
        } else {
            output
        };

        let mut result = json!({
            "command": command,
            "cwd": self.cwd.lock().unwrap().display().to_string(),
            "exit_code": exit_code,
            "output": output,
            "timed_out": timed_out,
            "truncated": truncated,
        });
        if aborted {
            result["error"] = json!("interrupted by user");
        }
        result
    }

    fn parse_marker(&self, line: &str, exit_code: &mut Option<i32>) -> bool {
        if !line.contains(&self.marker) {
            return false;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(pos) = parts.iter().position(|&p| p == self.marker) {
            // exit_code
            if let Some(ec_str) = parts.get(pos + 1) {
                if let Ok(ec) = ec_str.parse::<i32>() {
                    *exit_code = Some(ec);
                }
            }
            // cwd
            if let Some(cwd_slice) = parts.get(pos + 2..) {
                let new_cwd: String = cwd_slice.join(" ");
                if !new_cwd.is_empty() && Path::new(&new_cwd).is_dir() {
                    *self.cwd.lock().unwrap() = PathBuf::from(new_cwd);
                }
            }
        }
        true
    }

    fn trailer(&self) -> String {
        if self.is_windows {
            format!(
                "\"\n{marker} $(if ($?) {{0}} else {{ if ($LASTEXITCODE) {{$LASTEXITCODE}} else {{1}} }}) $(pwd)\"\n",
                marker = self.marker
            )
        } else {
            format!(
                "printf \"\\n%s %s %s\\n\" \"{marker}\" \"$?\" \"$PWD\"\n",
                marker = self.marker
            )
        }
    }

    fn interrupt_inner(&self, _proc: &ProcessHandle) {
        #[cfg(unix)]
        {
            let pid = _proc.child.id();
            // Find and SIGINT child processes of the shell
            if let Ok(output) = StdCommand::new("pgrep")
                .args(["-P", &pid.to_string()])
                .output()
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for child_pid_str in stdout.split_whitespace() {
                    if let Ok(child_pid) = child_pid_str.parse::<i32>() {
                        unsafe {
                            libc::kill(child_pid, libc::SIGINT);
                        }
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            // No reliable "interrupt one command" on PowerShell; the timeout path
            // in run() kills the shell tree instead and next run respawns.
        }
    }

    pub fn interrupt_now(&self) {
        self.abort.store(true, Ordering::SeqCst);
    }

    /// Run a background command (detached process, not the persistent shell).
    pub fn run_background(&self, command: &str) -> Value {
        let mut counter = self.bg_counter.lock().unwrap();
        *counter += 1;
        let task_id = format!("bg-{counter}");
        drop(counter);

        match BackgroundTask::new(
            task_id.clone(),
            command,
            &self.cwd.lock().unwrap(),
            &self.envs,
        ) {
            Ok(task) => {
                self.bg_tasks.lock().unwrap().insert(task_id.clone(), task);
                json!({
                    "task_id": task_id,
                    "command": command,
                    "status": "running",
                    "note": "use shell_task_output to read its output, shell_task_kill to stop it"
                })
            }
            Err(e) => json!({"error": e}),
        }
    }

    pub fn background_output(&self, task_id: &str) -> Value {
        let mut guard = self.bg_tasks.lock().unwrap();
        let task = match guard.get_mut(task_id) {
            Some(t) => t,
            None => return json!({"error": format!("unknown task: {task_id}")}),
        };
        let output = task.read_new();
        let truncated = output.len() > self.max_output_chars;
        let output = if truncated {
            output[output.len().saturating_sub(self.max_output_chars)..].to_string()
        } else {
            output
        };
        let status = if task.exit_code().is_some() {
            "exited"
        } else {
            "running"
        };
        json!({
            "task_id": task_id,
            "status": status,
            "exit_code": task.exit_code(),
            "output": output,
            "truncated": truncated,
        })
    }

    pub fn background_kill(&self, task_id: &str) -> Value {
        let mut guard = self.bg_tasks.lock().unwrap();
        let task = match guard.get_mut(task_id) {
            Some(t) => t,
            None => return json!({"error": format!("unknown task: {task_id}")}),
        };
        task.kill();
        let status = if task.exit_code().is_some() {
            "killed"
        } else {
            "running"
        };
        json!({
            "task_id": task_id,
            "status": status,
            "exit_code": task.exit_code(),
        })
    }
}

// ---------------------------------------------------------------------------
// Tool schemas and functions
// ---------------------------------------------------------------------------

fn run_shell_schema() -> ToolSchema {
    ToolSchema::new("run_shell", Some(
        "Run a shell command in the persistent session (cwd and env persist across calls). Output longer than the limit keeps the END. Set run_in_background for long-running processes like dev servers, then poll with shell_task_output."
    ), Some(json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "The command to run." },
            "description": { "type": "string", "description": "Short human-readable summary, shown in approval prompts." },
            "timeout_seconds": { "type": "integer", "description": "Max seconds to wait (default 120, max 600)." },
            "run_in_background": { "type": "boolean", "description": "Run detached and return a task_id immediately." }
        },
        "required": ["command"]
    })))
}

fn shell_task_output_schema() -> ToolSchema {
    ToolSchema::new(
        "shell_task_output",
        Some(
            "Read NEW output from a background task started with run_shell run_in_background=true.",
        ),
        Some(json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The task_id returned by run_shell." }
            },
            "required": ["task_id"]
        })),
    )
}

fn shell_task_kill_schema() -> ToolSchema {
    ToolSchema::new(
        "shell_task_kill",
        Some("Stop a background task started with run_shell run_in_background=true."),
        Some(json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The task_id returned by run_shell." }
            },
            "required": ["task_id"]
        })),
    )
}

/// Register shell tools (run_shell, shell_task_output, shell_task_kill) into `registry`.
/// The `executor` is an `Arc<LocalExecutor>` so it can be shared across tool calls.
pub fn register_all(registry: &mut ToolRegistry, executor: Arc<LocalExecutor>) {
    let exec1 = Arc::clone(&executor);
    let exec2 = Arc::clone(&executor);
    let exec3 = Arc::clone(&executor);

    let run_shell_fn: ToolFn = Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if command.is_empty() {
            return ToolResult::ok(json!({"error": "command is required"}));
        }
        let run_bg = args
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let timeout = args
            .get("timeout_seconds")
            .and_then(|v| v.as_i64())
            .filter(|&n| n > 0)
            .map(|n| Duration::from_secs(n as u64));

        if run_bg {
            ToolResult::ok(exec1.run_background(command))
        } else {
            ToolResult::ok(exec1.run(command, timeout))
        }
    });

    let task_output_fn: ToolFn = Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
        ToolResult::ok(exec2.background_output(task_id))
    });

    let task_kill_fn: ToolFn = Arc::new(move |args: Map<String, Value>| -> ToolResult {
        let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
        ToolResult::ok(exec3.background_kill(task_id))
    });

    registry.register(
        "run_shell",
        run_shell_fn,
        ToolSpec {
            risk_level: "high",
            category: "shell",
            parallel_safe: false,
        },
        Some(run_shell_schema()),
    );
    registry.register(
        "shell_task_output",
        task_output_fn,
        ToolSpec {
            risk_level: "low",
            category: "shell",
            parallel_safe: true,
        },
        Some(shell_task_output_schema()),
    );
    registry.register(
        "shell_task_kill",
        task_kill_fn,
        ToolSpec {
            risk_level: "low",
            category: "shell",
            parallel_safe: true,
        },
        Some(shell_task_kill_schema()),
    );
}
