//! MCP server (SPEC §8): streamable-HTTP MCP on `127.0.0.1:<port>` serving the
//! `ask_user` (blocking) and `notify_user` (fire-and-forget) tools, with a
//! bearer token persisted in `<data>/mcp.json` (401 without it, R-8.1).
//!
//! Filled in by T6.
