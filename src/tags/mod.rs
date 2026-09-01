pub mod atoms;
/// The generated-fixture suite (DESIGN §14). Test-only: it shells out to
/// ffmpeg to build the containers it runs on.
#[cfg(test)]
mod fixtures;
pub mod native;
pub mod plan;
pub mod probe;
pub mod write;
