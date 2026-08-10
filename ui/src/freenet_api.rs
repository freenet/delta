#[allow(dead_code)]
mod connection;
#[allow(dead_code)]
pub(crate) mod delegate;
#[allow(dead_code)]
pub(crate) mod delegate_migration;
/// Offline differential of the migration adoption against Delta's shipped sweep.
#[cfg(test)]
mod delegate_migration_differential;
#[allow(dead_code)]
mod operations;

pub use connection::{connect_to_freenet, ConnectionStatus, CONNECTION_STATUS};
pub use operations::{get_site, put_site};
