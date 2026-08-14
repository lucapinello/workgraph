//! Casa's own layer, kept OUT of upstream's files.
//!
//! `wg` is a general task graph; Casa is a household assistant built on it. Everything
//! here is Casa behaviour with no `wg` meaning, living at a path upstream does not have
//! so it can never become a merge conflict. See docs/UPSTREAM-DIVERGENCE.md.
pub mod command_gate;
pub mod digest;
pub mod dryruns;
pub mod elect;
pub mod feed_write;
pub mod group;
pub mod lifecycle;
pub mod one_shot_answers;
pub mod plan_edits;
pub mod remind;
pub mod reply_delivery;
pub mod telegram_photo;
