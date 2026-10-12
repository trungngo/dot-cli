//! Platform integrations. Adding a platform means implementing these traits plus a detection
//! rule, not changing the engine.

pub mod elevation;
pub mod exec;
pub mod http;
pub mod link;
pub mod pkg;
pub mod service;
pub mod shell;
pub mod sudo;
pub mod user;
pub mod winpath;

pub use exec::{Cmd, Runner, SystemRunner};
pub use link::{LinkCaps, LinkKind, LinkStrategy, SystemLinks};
pub use pkg::{PackageManager, Packages};
pub use service::ServiceManager;
pub use sudo::{Privilege, Sudo, SudoPrepare};
