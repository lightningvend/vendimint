//! # Vendimint
//!
//! A rust crate that provides a simple API for building applications that need to receive payments
//! over the bitcoin lightning network, but don't ever need to actually take control of the funds.
//!
//! ## API Overview
//!
//! The vendimint API has two device types: machines and managers.
//!
//! ## Key-Value Store Interface
//!
//! Vendimint provides a shared key-value store between machines and their managers for
//! application-specific data exchange:
//!
//! ```rust,no_run
//! use vendimint::{Machine, Manager, KvEntry, KvEntryAuthor};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! # let machine: Machine = todo!();
//! # let manager: Manager = todo!();
//! # let machine_id = todo!();
//! // Machine writes data
//! machine.set_kv_value(b"status", b"operational").await?;
//!
//! // Manager reads the data
//! let entry = manager.get_kv_value(&machine_id, b"status").await?;
//! if let Some(entry) = entry {
//!     println!("Machine status: {:?}", std::str::from_utf8(&entry.value)?);
//!     println!("Written by: {:?}", entry.author); // KvEntryAuthor::Machine
//! }
//!
//! // Manager writes configuration
//! manager.set_kv_value(&machine_id, b"config", b"debug_mode=true").await?;
//!
//! // Machine reads the configuration
//! let config = machine.get_kv_value(b"config").await?;
//! # Ok(())
//! # }
//! ```
//!
//! The KV store features:
//! - Automatic synchronization between paired devices
//! - Author tracking (which device wrote each entry)
//! - Support for arbitrary binary data
//! - Independent operation from payment processing
//! - Exact byte preservation (what you write is exactly what you read)
mod fedimint_wallet;
mod machine;
mod manager;
mod vendimint_iroh;

pub use machine::Machine;
pub use manager::Manager;
pub use vendimint_iroh::{KvEntry, KvEntryAuthor, MachineConfig};
