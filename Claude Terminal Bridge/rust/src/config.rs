use std::{env, path::PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub session_token: String,
    pub save_history: bool,
    pub sandbox_dir: PathBuf,
    pub attach_keyword: String,
}

impl Default for Config {
    fn default() -> Self {
        let sandbox_dir = env::var_os("CLAUDE_SANDBOX_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("sandbox"));

        let attach_keyword = env::var("CLAUDE_ATTACH_KEYWORD")
            .unwrap_or_else(|_| "attach".to_string())
            .trim()
            .to_string();

        Self {
            port: 8765,
            session_token: uuid::Uuid::new_v4().to_string(),
            save_history: false,
            sandbox_dir,
            attach_keyword: if attach_keyword.is_empty() {
                "attach".to_string()
            } else {
                attach_keyword
            },
        }
    }
}
