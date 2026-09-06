//! 単純ランクマッチシステム「Assay」（Pokémon UNITE 用）— lifecycle / persistence / discord / registration / queue / api。
#![forbid(unsafe_code)]

pub mod api;
pub mod discord;
pub mod lifecycle;
pub mod persistence;
pub mod queue;
pub mod rating;
pub mod registration;
