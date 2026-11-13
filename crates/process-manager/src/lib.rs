use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{error, info};

pub struct ManagedProcess {
    pub id: String,
    pub child: Child,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub log_file: PathBuf,
}

type ProcessMap = Arc<Mutex<HashMap<String, ManagedProcess>>>;

lazy_static::lazy_static! {
    static ref PROCESSES: ProcessMap = Arc::new(Mutex::new(HashMap::new()));
    static ref EXIT_CODES: Arc<Mutex<HashMap<String, i32>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

pub fn log_file_for(logs_dir: &Path, id: &str) -> PathBuf {
    logs_dir.join(format!("{}.log", id))
}

pub fn write_log(log_file: &Path, level: &str, message: &str) {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let line = format!("[{}] [{}] {}\n", timestamp, level, message);

    if let Err(e) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()))
    {
        error!("Failed to write log: {}", e);
    }
}

pub fn read_tail(log_file: &Path, limit_bytes: usize) -> String {
    match std::fs::read(log_file) {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(limit_bytes);
            String::from_utf8_lossy(&bytes[start..]).to_string()
        }
        Err(_) => String::new(),
    }
}

pub async fn start_managed(
    id: String,
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    env: HashMap<String, String>,
    log_file: PathBuf,
) -> Result<u32> {
    stop_managed(&id).await;
    EXIT_CODES.lock().unwrap().remove(&id);

    if !cwd.is_dir() {
        anyhow::bail!("Project directory not found: {}", cwd.display());
    }

    let mut cmd = build_command(&command, &args);
    cmd.current_dir(&cwd)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn()?;
    let pid = child.id();

    write_log(&log_file, "sys", &format!("spawned pid {:?}: {} {}", pid, command, args.join(" ")));

    // Capture stdout
    if let Some(stdout) = child.stdout.take() {
        let log_file = log_file.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                write_log(&log_file, "out", &line);
            }
        });
    }

    // Capture stderr
    if let Some(stderr) = child.stderr.take() {
        let log_file = log_file.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                write_log(&log_file, "err", &line);
            }
        });
    }

    let watch_log = log_file.clone();
    let proc = ManagedProcess {
        id: id.clone(),
        child,
        pid,
        exit_code: None,
        log_file,
    };

    PROCESSES.lock().unwrap().insert(id.clone(), proc);
    spawn_exit_watcher(id, watch_log);
    Ok(pid.unwrap_or(0))
}

fn build_command(command: &str, args: &[String]) -> Command {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", command]).args(args);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut cmd = Command::new(command);
        cmd.args(args);
        cmd
    }
}

fn spawn_exit_watcher(id: String, log_file: PathBuf) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            let status = PROCESSES
                .lock()
                .unwrap()
                .get_mut(&id)
                .and_then(|proc| proc.child.try_wait().transpose())
                .transpose();
            match status {
                Ok(Some(exit)) => {
                    let code = exit.code().unwrap_or(-1);
                    PROCESSES.lock().unwrap().remove(&id);
                    EXIT_CODES.lock().unwrap().insert(id.clone(), code);
                    write_log(&log_file, "sys", &format!("process exited with code {}", code));
                    break;
                }
                Ok(None) => continue,
                Err(e) => {
                    error!("Exit watcher failed for {}: {}", id, e);
                    break;
                }
            }
        }
    });
}

pub async fn stop_managed(id: &str) -> bool {
    let proc = PROCESSES.lock().unwrap().remove(id);
    let Some(proc) = proc else {
        return false;
    };

    info!("Stopping process {}", id);

    #[cfg(target_os = "windows")]
    {
        if let Some(pid) = proc.pid {
            let _ = Command::new("taskkill")
                .args(["/pid", &pid.to_string(), "/T", "/F"])
                .output()
                .await;
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut proc = proc;
        let _ = proc.child.kill().await;
    }

    true
}

pub fn is_running(id: &str, _pid: Option<i64>) -> bool {
    PROCESSES.lock().unwrap().contains_key(id)
}

pub fn get_exit_code(id: &str) -> Option<i32> {
    PROCESSES
        .lock()
        .unwrap()
        .get(id)
        .and_then(|p| p.exit_code)
        .or_else(|| EXIT_CODES.lock().unwrap().get(id).copied())
}
