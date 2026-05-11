//! OpenAI-compat backend client. Phase 1 step 2 fills this in (chat completions
//! + function calling). For now it's a stub.

pub mod chrome_bridge;
pub mod glm_mcp;
pub mod openai_compat;
pub mod vision;

pub use vision::vision_chat;
