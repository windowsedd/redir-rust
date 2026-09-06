//! Library crate backing the `redir-rust` binary. Splitting this out (thin
//! `main.rs` + real logic here) is what lets `tests/*.rs` integration tests
//! exercise the proxy/config/plugin modules directly instead of only being
//! able to spawn the compiled binary as a subprocess.

pub mod config;
pub mod conn_worker;
pub mod connections;
pub mod edit_config;
pub mod install;
pub mod notify;
pub mod plugin;
pub mod plugins;
pub mod proctitle;
pub mod proxy;
pub mod service_ctl;
pub mod shaping;
pub mod status;
pub mod udp_proxy;
pub mod version;
