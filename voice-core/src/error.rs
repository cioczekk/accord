use thiserror::Error;

#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Opus error: {0}")]
    Opus(#[from] opus::Error),
    #[error("Audio error: {0}")]
    Audio(String),
    #[error("Ticket error: {0}")]
    Ticket(String),
    #[error("Channel closed")]
    ChannelClosed,
    #[error("Invalid state: {0}")]
    InvalidState(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, VoiceError>;
