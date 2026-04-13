use std::os::fd::OwnedFd;

/// A Wayland message that can be encoded into the wire format.
pub trait Message {
    /// The operation code of the message.
    const OPCODE: u16;

    /// Encodes the message arguments.
    fn encode(self, encoder: &mut Encoder);
}

/// An encoder for Wayland message arguments.
#[derive(Debug)]
pub struct Encoder<'a>(&'a mut Buffer);

impl Encoder<'_> {
    /// Encodes a 32-bit signed integer argument.
    pub fn encode_int(&mut self, value: i32) {
        self.0.bytes.extend_from_slice(&value.to_ne_bytes());
    }

    /// Encodes a 32-bit unsigned integer argument.
    pub fn encode_uint(&mut self, value: u32) {
        self.0.bytes.extend_from_slice(&value.to_ne_bytes());
    }

    /// Encodes a fixed-point number argument.
    pub fn encode_fixed(&mut self, value: f64) {
        // Wayland uses a 24.8 signed fixed-point format.
        // Multiplying by 256.0 (2^8) shifts the fractional part into the integer domain.
        let fixed = (value * 256.0) as i32;
        self.0.bytes.extend_from_slice(&fixed.to_ne_bytes());
    }

    /// Encodes an object ID argument.
    pub fn encode_object(&mut self, id: u32) {
        self.0.bytes.extend_from_slice(&id.to_ne_bytes());
    }

    /// Encodes a new object ID argument.
    pub fn encode_new_id(&mut self, id: u32) {
        self.0.bytes.extend_from_slice(&id.to_ne_bytes());
    }

    /// Encodes a string argument.
    pub fn encode_string(&mut self, s: &str) {
        todo!()
    }

    /// Encodes a fixed-size array argument.
    pub fn encode_array(&mut self, array: &[u8]) {
        todo!()
    }

    /// Encodes a file descriptor argument.
    pub fn encode_fd(&mut self, fd: OwnedFd) {
        self.0.fds.push(fd);
    }
}

/// A buffer for constructing Wayland messages to be sent over the wire.
#[derive(Debug)]
pub struct Buffer {
    /// The outgoing bytes.
    bytes: Vec<u8>,

    /// The outgoing FDs.
    fds: Vec<OwnedFd>,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    /// Creates an empty `OutBuffer`.
    pub const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            fds: Vec::new(),
        }
    }

    /// Pushes a message into the buffer.
    pub fn push_message<M>(&mut self, object_id: u32, msg: M)
    where
        M: Message,
    {
        // Record the starting offset of the message.
        let msg_start = self.bytes.len();

        // Push the header with Word 1 (opcode and size) reserved, to be backfilled later.
        self.push_header_reserved(object_id);

        // Push arguments.
        msg.encode(&mut Encoder(self));

        // Calculate the message size.
        let size = (self.bytes.len() - msg_start) as u16;

        // Backfill Word 1 in the header.
        self.backfill_header(msg_start, M::OPCODE, size);
    }

    /// Returns the raw byte and FD buffers for flushing.
    pub fn as_raw_parts(&self) -> (&[u8], &[OwnedFd]) {
        (&self.bytes, &self.fds)
    }

    /// Clears the buffer.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.fds.clear();
    }

    fn push_header_reserved(&mut self, object_id: u32) {
        // Create an 8-byte header initialized to zero.
        // The last 4 bytes are naturally reserved for Word 1 (opcode & size).
        let mut header = [0u8; 8];

        // Populate the first 4 bytes with Word 0 (Object ID).
        header[..4].copy_from_slice(&object_id.to_ne_bytes());

        // Push the entire 8-byte header into the buffer.
        self.bytes.extend_from_slice(&header);
    }

    fn backfill_header(&mut self, msg_start: usize, opcode: u16, size: u16) {
        // Word 1: size occupies the upper 16 bits, opcode the lower 16 bits.
        let word1 = ((size as u32) << 16) | (opcode as u32);

        // Get Word 1 slice in the header.
        //
        // SAFETY: Word 1 is reserved in the header.
        let target = unsafe { self.bytes.get_unchecked_mut(msg_start + 4..msg_start + 8) };

        // Backfill the reserved Word 1 with the combined opcode and size.
        target.copy_from_slice(&word1.to_ne_bytes());
    }
}
