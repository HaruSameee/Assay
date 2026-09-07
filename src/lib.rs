//! 単純ランクマッチシステム「Assay」（Pokémon UNITE 用）— lifecycle / persistence / matchmaker / discord / registration / queue / status / admin / api。
#![forbid(unsafe_code)]

pub mod admin;
pub mod api;
pub mod discord;
pub mod lifecycle;
pub mod match_screen;
pub mod matchmaker;
pub mod persistence;
pub mod queue;
pub mod rating;
pub mod registration;
pub mod status;
