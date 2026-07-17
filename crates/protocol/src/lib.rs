//! Generated Connect service/message types from `proto/`, shared by every
//! Rust service that speaks one of these APIs (currently just sync-daemon;
//! the controller will add its own service definitions here later). Go and
//! TypeScript clients (the bot, the future website) generate from the same
//! `proto/` source in their own repos -- this crate is only the Rust side.

pub mod proto {
    connectrpc::include_generated!();
}
