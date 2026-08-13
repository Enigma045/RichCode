pub enum CommandAction {
    Help,
    Status,
    Select(String),
    Reconnect,
    Attach(Option<String>),
    Clear,
    Quit,
    RichBot(String),
    Unknown(String),
}

pub fn parse_command(input: &str) -> Option<CommandAction> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();

    // Check attach / /attach command
    if parts[0].eq_ignore_ascii_case("attach") || parts[0].eq_ignore_ascii_case("/attach") {
        if parts.len() > 1 {
            return Some(CommandAction::Attach(Some(parts[1..].join(" "))));
        } else {
            return Some(CommandAction::Attach(None));
        }
    }

    if !trimmed.starts_with('/') {
        return None;
    }

    match parts[0] {
        "/help" => Some(CommandAction::Help),
        "/status" => Some(CommandAction::Status),
        "/select" => {
            if parts.len() > 1 {
                Some(CommandAction::Select(parts[1..].join(" ")))
            } else {
                Some(CommandAction::Select("".to_string()))
            }
        }
        "/reconnect" => Some(CommandAction::Reconnect),
        "/clear" => Some(CommandAction::Clear),
        "/quit" => Some(CommandAction::Quit),
        "/richbot" | "/rb" => {
            let prompt = parts[1..].join(" ");
            Some(CommandAction::RichBot(prompt))
        }
        cmd => Some(CommandAction::Unknown(cmd.to_string())),
    }
}
