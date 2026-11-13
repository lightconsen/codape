//! Codape daemon server — WebSocket sessions and SDK structured I/O.

#![warn(missing_docs)]

pub mod auth;
pub mod control_plane;
pub mod filter;
pub mod gateway;
pub mod persistence;
pub mod session;
pub mod teleport;

pub use auth::{
    AuthError, LISTEN_HOST_ENV, SERVICE_TOKENS_ENV, ServiceAuth, ensure_exposure_is_authenticated,
    exposure_note, is_loopback, resolve_bind_addr,
};
pub use control_plane::{
    AssignmentEntry, CapacityReport, ControlPlaneServer, ControlPlaneState, RegisterRequest,
    RegisterResponse,
};
pub use filter::{CommandFilter, SafetyLevel};
pub use gateway::GatewayServer;
pub use persistence::{PersistedSession, SessionStore};
pub use session::{
    DaemonServer, ServerConfig, Session, SessionRole, WsEvent, WsRequest, WsRequestKind,
    WsResponse, WsResponseKind,
};
