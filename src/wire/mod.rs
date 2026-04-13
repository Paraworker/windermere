pub mod rx;
pub mod tx;

/// The maximum size of a Wayland message, including the header and arguments.
const MAX_MESSAGE_SIZE: usize = 4096;
