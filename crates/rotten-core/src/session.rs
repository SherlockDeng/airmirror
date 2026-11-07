use serde::{Deserialize, Serialize};

/// State of an active or pending mirror session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Disconnected,
    Connecting,
    Pairing,
    Authenticating,
    SettingUp,
    Streaming,
    Stopping,
}

/// Runtime mirror session metadata.
#[derive(Debug, Clone)]
pub struct MirrorSession {
    pub state: SessionState,
    pub session_id: Option<String>,
    pub stream_port: Option<u16>,
    pub control_port: Option<u16>,
}

impl Default for MirrorSession {
    fn default() -> Self {
        Self {
            state: SessionState::Disconnected,
            session_id: None,
            stream_port: None,
            control_port: None,
        }
    }
}

impl MirrorSession {
    pub fn transition(&mut self, state: SessionState) {
        self.state = state;
    }
}
