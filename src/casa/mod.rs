//! Casa's own layer, kept OUT of upstream's files.
//!
//! `wg` is a general task graph; Casa is a household assistant built on it. Everything
//! here is Casa behaviour with no `wg` meaning, living at a path upstream does not have
//! so it can never become a merge conflict. See docs/UPSTREAM-DIVERGENCE.md.
pub mod digest;
pub mod dryruns;
pub mod elect;
pub mod one_shot_answers;
pub mod remind;
pub mod reply_delivery;
pub mod telegram_photo;
