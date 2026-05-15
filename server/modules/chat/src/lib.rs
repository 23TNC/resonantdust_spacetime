// lib.rs — chat module.
//
// Self-contained world-chat database. Owns `chat_messages` (public,
// append-only) and `chat_retention` (private, scheduled). Does not
// know about `players`, `cards`, `zones`, or any gameplay state —
// callers pass `sender_player_id` + `sender_name` explicitly into
// `send_chat_message`. Eventually a sidecar (or chat-side session
// table) will be the trust boundary that prevents spoofing.

pub mod chat;
pub mod packed;
pub mod sequence;
