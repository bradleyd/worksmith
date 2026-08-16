//! Worksmith — a minimal terminal coding-agent harness.
//!
//! The library surface is intentionally small so the binary (and, later, an
//! SDK or spawned workers) can drive the same agent loop. See `PLAN.md`.

pub mod agent;
pub mod config;
pub mod event;
pub mod llm;
pub mod memory;
pub mod session;
pub mod tools;
pub mod validation;
