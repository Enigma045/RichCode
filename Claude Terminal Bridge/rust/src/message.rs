#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub role: String,
    pub content: String,
    pub status: MessageStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageStatus {
    Streaming,
    Complete,
}
