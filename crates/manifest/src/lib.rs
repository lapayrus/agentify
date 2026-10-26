//! Strict parse and normalization of a Manifest into a normalized spec.
//!
//! Pure: this crate never touches the filesystem, the database, or the
//! network. The daemon hands it YAML text and gets back either a normalized
//! [`Manifest`] or a [`ManifestError`] carrying the `path` of the offending
//! field, so an Agent can fix the right line (ADR-0002: the Manifest is the
//! source of truth).
//!
//! The schema is closed: unknown fields are rejected at every level, because a
//! hand-written manifest's main failure mode is a typo that silently does
//! nothing.

mod error;
mod model;
mod parse;

pub use error::ManifestError;
pub use model::{
    App, Build, Credentials, Database, Healthcheck, Manifest, Port, Resource, ResourceKind, Route,
    Secret, SecretKind, DEFAULT_DATABASE_VERSION, DEFAULT_DOCKERFILE, DEFAULT_HEALTHCHECK_INTERVAL,
    MANIFEST_VERSION,
};
pub use parse::{parse, MAX_NAME_LEN};
