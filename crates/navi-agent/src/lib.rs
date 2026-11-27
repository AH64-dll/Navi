//! Team 2: Core Agent Harness, Built-in Tools, Sessions & Compaction
//! (ported from `packages/agent` and `packages/coding-agent` core).

pub mod agent_loop;
pub mod compaction;
pub mod native_harness;
pub mod prompt;
pub mod session_store;
pub mod state;
pub mod tool_bash;
pub mod tool_edit;
pub mod tool_read;
pub mod tool_router;
pub mod tool_write;

pub use navi_types::{
    ContentBlock, Message, ModelRef, NaviError, NaviResult, Role, SessionId, StopReason,
    StreamEvent, ToolCall, ToolDefinition, ToolResult, Usage,
};
