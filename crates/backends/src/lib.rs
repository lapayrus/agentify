//! The Backend contract and the scripted fake that implements it.
//!
//! A **Backend** is the runtime substrate that hosts an Environment (GLOSSARY):
//! a WSL distro, an Apple container microVM, or a Lima VM. This crate holds the
//! substitutable interface — environment-scoped, lifecycle-only — so every
//! adapter behaves the same and the daemon never learns which substrate it is
//! talking to.
//!
//! Three things live here:
//!
//! * [`Backend`], the async trait object the daemon holds. Object-safe by
//!   construction: every fallible call returns a boxed future, so the daemon
//!   holds an `Arc<dyn Backend>` and tests substitute the fake.
//! * [`FakeBackend`], a deterministic implementation: start binds a TCP
//!   listener on each leased host port so the daemon can health-probe it, stop
//!   releases them, and failures and degradation are scriptable.
//! * [`run_conformance`], a reusable suite over any [`Backend`]. The fake runs
//!   it in CI on every OS today; the WSL adapter runs it on Windows later.
//!
//! The daemon — never the Backend — owns port leases and the health decision:
//! it allocates a lease, passes it in the spec, and probes the leased port
//! itself. That is what makes every substrate behave the same.

pub mod conformance;
pub mod contract;
pub mod error;
pub mod fake;
pub mod spec;

pub use conformance::{run_conformance, Case, Outcome, Report};
pub use contract::{
    Backend, BackendEvent, BackendInfo, BackendStatus, BoxFuture, CheckpointId, EventStream,
    ExecChunk, ExecSession, ExecSpec, LogLine, LogOptions, LogStream, ResourceStat, ResourceStatus,
    Stats, STATE_ABSENT, STATE_DEGRADED, STATE_FAILED, STATE_PROVISIONING, STATE_RUNNING,
    STATE_STARTING, STATE_STOPPED,
};
pub use error::{BackendError, Result};
pub use fake::{FakeBackend, FakeFault, FakeScript};
pub use spec::{EnvironmentSpec, HealthcheckSpec, PortLease, ResourceSpec};
