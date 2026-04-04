use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

pub fn validate_file(tool_path: &Path, original_path: &Path) -> Vec<String> {
    let ext = original_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let path_str = tool_path.to_str().unwrap_or("");

    enum CheckMode {
        ExitCode,
        NonEmptyStdout,
    }

    let args: Option<(&str, Vec<&str>, CheckMode)> = match ext {
        "rs" => Some(("rustfmt", vec!["--check", path_str], CheckMode::ExitCode)),
        "tf" | "hcl" => Some((
            "terraform",
            vec!["fmt", "-check", path_str],
            CheckMode::ExitCode,
        )),
        "py" => Some((
            "python",
            vec!["-m", "py_compile", path_str],
            CheckMode::ExitCode,
        )),
        "json" => Some((
            "python3",
            vec!["-m", "json.tool", path_str],
            CheckMode::ExitCode,
        )),
        "js" | "jsx" => Some(("node", vec!["--check", path_str], CheckMode::ExitCode)),
        "go" => Some(("gofmt", vec!["-l", path_str], CheckMode::NonEmptyStdout)),
        "sh" | "bash" => Some(("bash", vec!["-n", path_str], CheckMode::ExitCode)),
        _ => None,
    };

    let (bin, cmd_args, check_mode) = match args {
        Some(a) => a,
        None => return vec![],
    };

    // Check if binary exists before attempting to run
    let which_check = Command::new("which").arg(bin).output();
    let bin_exists = which_check.map(|o| o.status.success()).unwrap_or(false);

    if !bin_exists {
        return vec![format!(
            "Advisory: {bin} not found — skipping {ext} syntax check"
        )];
    }

    // Run with timeout via mpsc channel + recv_timeout
    let cmd_name = bin;
    let (tx, rx) = mpsc::channel();
    let owned_cmd = cmd_name.to_string();
    let owned_args: Vec<String> = cmd_args.iter().map(|s| s.to_string()).collect();

    std::thread::spawn(move || {
        let result = Command::new(&owned_cmd).args(&owned_args).output();
        let _ = tx.send(result);
    });

    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(output)) => match check_mode {
            CheckMode::ExitCode => {
                if output.status.success() {
                    vec![]
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let detail = if !stderr.trim().is_empty() {
                        stderr.trim().to_string()
                    } else if !stdout.trim().is_empty() {
                        stdout.trim().to_string()
                    } else {
                        format!("exit code {}", output.status)
                    };
                    vec![format!("Advisory ({cmd_name}): {detail}")]
                }
            }
            CheckMode::NonEmptyStdout => {
                if !output.stdout.trim_ascii().is_empty() {
                    vec!["Advisory (gofmt): file needs formatting".to_string()]
                } else if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let detail = if !stderr.trim().is_empty() {
                        stderr.trim().to_string()
                    } else {
                        format!("exit code {}", output.status)
                    };
                    vec![format!("Advisory ({cmd_name}): {detail}")]
                } else {
                    vec![]
                }
            }
        },
        Ok(Err(e)) => vec![format!("validator '{}' failed: {}", cmd_name, e)],
        Err(_) => vec![format!("validator '{}' timed out after 2s", cmd_name)],
    }
}
