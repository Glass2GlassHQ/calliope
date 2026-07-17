//! Engine-neutral core of the calliope harness. Engines (ffmpeg, GStreamer,
//! g2g, ...) are black-box subprocesses behind the [`engine::Engine`] trait;
//! this crate owns scenarios, the corpus, subprocess execution, frame
//! hashing, and comparison. Nothing here may know about a specific engine.

pub mod compare;
pub mod corpus;
pub mod engine;
pub mod fault;
pub mod framehash;
pub mod report;
pub mod runner;
pub mod scenario;

pub use engine::{Engine, EngineInfo, Invocation, OutputSpec};
pub use scenario::Scenario;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Parse(String),
    #[error("engine {engine}: {message}")]
    Engine { engine: String, message: String },
    #[error("corpus: {0}")]
    Corpus(String),
}

pub type Result<T> = std::result::Result<T, Error>;
