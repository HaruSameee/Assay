//! Append-only ally-block relations.
//!
//! A block only prevents two players from being placed on the same team. It
//! never affects opponents and the current relation is derived from the latest
//! event for each player pair.

pub mod model;
pub mod repository;

pub use model::{
    AdminBlockOverrideError, BlockKind, BlockSearchResult, CurrentBlock,
    DEFAULT_BLOCK_COOLDOWN_DAYS, IncomingBlock, MAX_BLOCK_COOLDOWN_DAYS, ReceivedBlockCount,
    UserBlockError, cooldown_ends_at,
};
pub use repository::BlockRepository;
