//! Team 3: Persona, Mail, Desktop/Windows, IPC, ACP & Pi RPC Connectors
//! (ported from `apps/navi/workers/{persona,ipc,mail,desktop,windows,acp_client,pi_rpc}.py`).

pub mod acp;
pub mod acp_runner;
pub mod desktop;
pub mod ipc;
pub mod mail;
pub mod mail_client;
pub mod persona;
pub mod pi_rpc;
pub mod pi_rpc_runner;
pub mod windows;

pub use navi_types::{NaviError, NaviResult};
