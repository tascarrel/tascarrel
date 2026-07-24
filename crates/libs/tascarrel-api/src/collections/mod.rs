//! Copy-on-write collections used by generated API types.

mod string;
mod vector;

pub use string::ArcStr;
pub use vector::ArcVec;
