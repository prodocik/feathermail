//! Composition root: the one place that knows both `feathermail-core` and
//! `feathermail-providers` (T-078).
//!
//! # Why this crate exists
//!
//! D9 draws the layering as `GTK UI / MCP → Core API → SQLite / Queue /
//! Providers`, i.e. Core drives the providers. In Cargo that arrow cannot
//! be a dependency: `feathermail-providers` depends on `feathermail-core`
//! (it implements Core's traits — `MailConnector`, `MailProvider`,
//! `RemoteLocator`), so `core → providers` would be a cycle. T-075 already
//! hit this and resolved it by moving the *traits* down into Core.
//!
//! What was still missing is the other half: somebody has to construct the
//! concrete provider and hand it to Core. That somebody cannot be
//! `crates/core` (cycle) and must not be `crates/app` (the GTK shell would
//! then know IMAP, which is what D9 forbids — and why the "Add mailbox"
//! wizard is still on a fixture, see T-074's four fake pockets).
//!
//! So it is this crate: it sits *above* both, depends on both, and is the
//! only crate `crates/app` reaches for live mail. `crates/app` does not
//! depend on `crates/providers` and still must not.
//!
//! ```text
//!            crates/app  (GTK shell)
//!                 │
//!                 ▼
//!        crates/service  ── this crate: wiring + background worker
//!          │           │
//!          ▼           ▼
//!   crates/core   crates/providers ──► crates/core
//! ```
//!
//! # D11 and D12
//!
//! D12 says one process in the MVP: sync is a background **thread** here,
//! not a separate daemon binary (that's Phase 2, T-109). D11 says the GTK
//! thread does no work over 16 ms, so nothing in this crate may be called
//! synchronously from a GTK callback: the shell talks to the worker by
//! sending it a command and receiving events back.

pub mod autoconfig;
pub mod bodylog;
pub mod connector;
pub mod provider_factory;
pub mod provision;
pub mod worker;

pub use autoconfig::{spawn_autoconfig, AutoconfigOutcome, LOOKUP_TIMEOUT};
pub use connector::ConnectorKind;
pub use provider_factory::{ImapProviderFactory, MailSession, ProviderFactory};
pub use provision::{spawn_provision, OauthProvider, ProvisionRequest};
pub use worker::{start, start_deferred, SyncEvent, SyncHandle, Viewport};

pub fn crate_name() -> &'static str {
    "feathermail-service"
}
