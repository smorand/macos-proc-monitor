//! macos-proc-core — collection + web dashboard for macOS process metrics.

pub mod collect;
pub mod web;

pub use collect::{collect_loop, default_dir, init_logging, CollectConfig};
pub use web::serve_web;
