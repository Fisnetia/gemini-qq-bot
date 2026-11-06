//! Image handling: allowlisted download, bounded decode, and shrink-until-it-fits
//! encoders for the two consumers. The gate reads pictures once and turns them into text;
//! the agent never receives image bytes, so there is no collage and no staged image file.

pub mod collage;
pub mod download;
pub mod resize;
pub mod workspace;

pub use collage::{MAX_BATCH_IMAGES, MAX_EDGE, Transport, fit_for_agy, fit_for_vision};
pub use download::{DownloadedImage, HttpFetcher, ImageFetcher, MediaError, check_url};
pub use resize::{PreparedImage, prepare};
pub use workspace::SandboxWorkspace;
