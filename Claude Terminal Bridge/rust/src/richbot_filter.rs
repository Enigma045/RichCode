//! RichBot bridge used to filter Claude responses for sandbox file requests
//! and write generated files directly into the sandbox directory.

use serde::Deserialize;
use std::{env, fs, path::{Path, PathBuf}, process::Stdio};
use tokio::{io::AsyncWriteExt, process::Command};

// -----------------------------------------------------------------------------
// DATA TYPES
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RichBotResult {
    #[serde(default)]
    files: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct GeneratedFile {
    pub filename: String,
    pub code: String,
}

#[derive(Debug, Deserialize)]
pub struct GeneratedFiles {
    pub files: Vec<GeneratedFile>,
}

#[derive(Debug)]
pub enum ClaudeResponse {
    Text(String),
    FilesWritten(Vec<String>),
}

// -----------------------------------------------------------------------------
// RICHBOT FILTER
// -----------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct RichBotFilter {
    command: Option<String>,
}

impl RichBotFilter {
    pub fn from_environment() -> Self {
        Self {
            command: env::var("RICHBOT_COMMAND")
                .ok()
                .filter(|v| !v.trim().is_empty()),
        }
    }

    /// Automatically determines whether Claude returned:
    /// 1. Generated-file JSON object (written directly to the sandbox)
    /// 2. Normal text message
    pub fn process_claude_response(
        &self,
        message: &str,
        sandbox: &Path,
    ) -> Result<ClaudeResponse, String> {
        let trimmed = message.trim();

        // 1. Try complete response as raw JSON
        if let Ok(generated) = serde_json::from_str::<GeneratedFiles>(trimmed) {
            if !generated.files.is_empty() {
                let written = write_generated_files(generated.files, sandbox)?;
                return Ok(ClaudeResponse::FilesWritten(written));
            }
        }

        // 2. Try Markdown ```json code block
        if let Some(json) = extract_code_block(trimmed) {
            if let Ok(generated) = serde_json::from_str::<GeneratedFiles>(json) {
                if !generated.files.is_empty() {
                    let written = write_generated_files(generated.files, sandbox)?;
                    return Ok(ClaudeResponse::FilesWritten(written));
                }
            }
        }

        // 3. Search for embedded JSON object containing "files"
        if let Some(json) = extract_files_json(trimmed) {
            if let Ok(generated) = serde_json::from_str::<GeneratedFiles>(&json) {
                if !generated.files.is_empty() {
                    let written = write_generated_files(generated.files, sandbox)?;
                    return Ok(ClaudeResponse::FilesWritten(written));
                }
            }
        }

        // 4. Normal Claude text response
        Ok(ClaudeResponse::Text(message.to_string()))
    }

    /// Extracts paths to existing sandbox files using RichBot AI engine
    /// (or external process if RICHBOT_COMMAND is set).
    pub async fn extract_paths(
        &self,
        message: &str,
        sandbox: &Path,
    ) -> Result<Vec<String>, String> {
        let raw_files = if let Some(command) = &self.command {
            self.extract_paths_via_command(command, message).await?
        } else {
            let msg = message.to_string();
            tokio::task::spawn_blocking(move || {
                richbot::extract_file_paths(&msg)
            })
            .await
            .map_err(|e| format!("RichBot task join error: {}", e))??
        };

        Ok(validate_paths(raw_files, sandbox))
    }

    async fn extract_paths_via_command(
        &self,
        command: &str,
        message: &str,
    ) -> Result<Vec<String>, String> {
        let prompt = format!(
            "You are RichBot, a file-request filter. Analyze Claude's message below. \
             Return ONLY JSON in this exact shape: {{\"files\":[\"relative/path\"]}}. \
             Return an empty files array if Claude does not need any local files.\n\n\
             Claude message:\n{}",
            message
        );

        let mut child = Command::new(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start RichBot '{}': {}", command, e))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .map_err(|e| format!("could not send message to RichBot: {}", e))?;

            stdin.shutdown().await.ok();
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| format!("RichBot process failed: {}", e))?;

        if !output.status.success() {
            return Err(format!(
                "RichBot exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        let result: RichBotResult = serde_json::from_str(stdout.trim())
            .map_err(|e| format!("RichBot returned invalid JSON: {}. Output: {}", e, stdout.trim()))?;

        Ok(result.files)
    }
}

// -----------------------------------------------------------------------------
// HELPER FUNCTIONS
// -----------------------------------------------------------------------------

fn clean_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        p.to_path_buf()
    }
}

fn extract_code_block(message: &str) -> Option<&str> {
    let message = message.trim();

    let start = if let Some(pos) = message.find("```json") {
        pos + "```json".len()
    } else if let Some(pos) = message.find("```JSON") {
        pos + "```JSON".len()
    } else if let Some(pos) = message.find("```") {
        pos + "```".len()
    } else {
        return None;
    };

    let remaining = message[start..].trim_start();
    let end = remaining.find("```")?;

    Some(remaining[..end].trim())
}

fn extract_files_json(message: &str) -> Option<String> {
    let marker = "\"files\"";
    let marker_pos = message.find(marker)?;

    let before = &message[..marker_pos];
    let start = before.rfind('{')?;

    let bytes = message.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for i in start..bytes.len() {
        let ch = bytes[i] as char;

        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    return Some(message[start..=i].to_string());
                }
            }
            _ => {}
        }
    }

    None
}

/// Writes generated files safely into the sandbox directory.
pub fn write_generated_files(
    files: Vec<GeneratedFile>,
    sandbox: &Path,
) -> Result<Vec<String>, String> {
    if !sandbox.exists() {
        let _ = fs::create_dir_all(sandbox);
    }

    let root_canonical = fs::canonicalize(sandbox).map_err(|e| {
        format!("could not resolve sandbox directory '{}': {}", sandbox.display(), e)
    })?;
    let root = clean_path(&root_canonical);

    let mut written = Vec::new();

    for file in files {
        let filename = file.filename.trim();

        if filename.is_empty() {
            return Err("generated file has an empty filename".to_string());
        }

        if filename.len() > 1024 || filename.contains('\n') || filename.contains('\r') {
            return Err(format!("invalid filename: {}", filename));
        }

        let relative = Path::new(filename);

        if relative.is_absolute() {
            return Err(format!("absolute paths are not allowed: {}", filename));
        }

        if relative.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            return Err(format!("path traversal is not allowed: {}", filename));
        }

        let destination = root.join(relative);

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                format!("could not create directory '{}': {}", parent.display(), e)
            })?;

            let canonical_parent = fs::canonicalize(parent).map_err(|e| {
                format!("could not resolve directory '{}': {}", parent.display(), e)
            })?;
            let clean_parent = clean_path(&canonical_parent);

            if !clean_parent.starts_with(&root) {
                return Err(format!("file would escape sandbox: {}", filename));
            }
        }

        log::info!("[FILE-WRITER] Writing generated file to {}", destination.display());

        fs::write(&destination, &file.code).map_err(|e| {
            format!("could not write file '{}': {}", destination.display(), e)
        })?;

        log::info!(
            "[FILE-WRITER] Successfully wrote {} ({} bytes)",
            destination.display(),
            file.code.len()
        );

        let relative_path = destination.strip_prefix(&root).unwrap_or(relative);

        let normalized = relative_path.to_string_lossy().replace('\\', "/");
        if !written.contains(&normalized) {
            written.push(normalized);
        }
    }

    Ok(written)
}

fn validate_paths(files: Vec<String>, sandbox: &Path) -> Vec<String> {
    if !sandbox.exists() {
        let _ = fs::create_dir_all(sandbox);
    }

    let root_canonical = match fs::canonicalize(sandbox) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let root = clean_path(&root_canonical);

    let mut out = Vec::new();

    for raw in files {
        let raw = raw.trim();

        if raw.is_empty() || raw.len() > 1024 || raw.contains('\n') || raw.contains('\r') {
            continue;
        }

        let candidate = root.join(raw);
        let Ok(real_canonical) = fs::canonicalize(&candidate) else {
            continue;
        };
        let real = clean_path(&real_canonical);

        if real.starts_with(&root) && real.is_file() {
            if let Ok(rel) = real.strip_prefix(&root) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if !out.contains(&rel) {
                    out.push(rel);
                }
            }
        }
    }

    out
}
