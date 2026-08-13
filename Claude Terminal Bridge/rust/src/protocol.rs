use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        client: String,
        version: String,
        token: Option<String>,
    },
    AssistantMessage {
        conversation_id: String,
        message_id: String,
        role: String,
        content: String,
        status: String,
        #[serde(default)]
        historical: bool,
    },
    RekeyConversation {
        old_conversation_id: String,
        new_conversation_id: String,
    },
    ConversationActive {
        conversation_id: String,
    },
    Diagnostic {
        message: String,
    },
    Ping,
    Pong,
    Disconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    HelloAck {
        server: String,
        version: String,
    },
    SendMessage {
        content: String,
    },
    AttachFile {
        path: String,
        prompt: String,
    },
    Ping,
    Error {
        message: String,
    },
}
