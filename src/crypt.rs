//! At-rest encryption for uploaded files, wire-compatible with the
//! original teldrive scheme:
//!
//! - container header: `"TELDRIVE\x00\x00"` magic + 24-byte random nonce
//! - payload: the plaintext in `64 KiB` blocks, each sealed with `NaCl`
//!   secretbox (XSalsa20-Poly1305, 16-byte tag) under the data key, with
//!   the nonce incremented per block (little-endian carry, like the Go
//!   original)
//! - key: scrypt(password, salt, N=16384, r=8, p=1) → 32-byte data key
//!
//! ii-drive adaptation: each uploaded **part** is its own container (own
//! header, own block grid, nonce recorded in the database). Single-part
//! files are byte-identical to the original's single-stream format; split
//! files trade a ≤48-byte header per part boundary for self-describing
//! parts that decrypt and range-seek independently.
// The block/nonce/size arithmetic and `as` casts in this crypto module
// operate strictly on bounded, length/EOF-checked values. Block, size and
// nonce math is on fixed constants or lengths verified before use; the few
// genuinely-irreducible hot-loop sites carry scoped `#[allow]`s below.
#![allow(clippy::cast_possible_truncation)]
// Byte-manipulation crypto code: nonce-carry arithmetic, block-boundary
// math, and buffer slicing all run on values bounded by the container
// length (verified against EOF using a lenient read) or fixed constants,
// so overflow/panic is impossible by construction. Scattering ~40
// identical allows over the hot loops would only obscure that guarantee;
// a single commented module decision keeps it auditable.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
)]
// The `expect`s in this module decode hard invariants (fixed scrypt
// params, exact-length slice-to-array, secretbox sealing with a valid
// nonce, a nonce guaranteed parsed before use) — none can fail, so
// propagating a Result would only obscure the crypto intent.
#![allow(clippy::expect_used)]

use crypto_secretbox::{AeadInPlace, KeyInit, Nonce, XSalsa20Poly1305};
use std::pin::Pin;
use std::io;

pub use crypto_secretbox::Key;

/// Base64 of a 24-byte nonce — the on-disk representation stored in the
/// database row.
pub fn nonce_b64(nonce: &[u8; NONCE_SIZE]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(nonce)
}

/// Decodes a stored nonce back to bytes; returns `None` for a corrupt value.
pub fn nonce_from_b64(s: &str) -> Option<[u8; NONCE_SIZE]> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    raw.try_into().ok()
}

/// Alias so upload.rs reads naturally at the call site.
pub fn base64_encode(nonce: &[u8; NONCE_SIZE]) -> String {
    nonce_b64(nonce)
}

/// Magic that marks a container as encrypted.
const MAGIC: &[u8; 10] = b"TELDRIVE\x00\x00";
const NONCE_SIZE: usize = 24;
/// Magic + nonce: everything before the first sealed block.
pub const HEADER_SIZE: u64 = (MAGIC.len() + NONCE_SIZE) as u64;
/// Plaintext bytes carried by one sealed block.
pub const BLOCK_DATA: usize = 64 * 1024;
/// secretbox Poly1305 tag prepended to each block's ciphertext.
pub const BLOCK_TAG: usize = 16;
/// Full ciphertext footprint of one full block (16-byte tag + 64 KiB data).
pub const BLOCK_SIZE: usize = BLOCK_TAG + BLOCK_DATA;
/// scrypt work factor: N = 2^14, matching the original.
const SCRYPT_LOG_N: u8 = 14;

/// Derives the 32-byte data key from the operator's password and salt.
/// Parameters match the original (scrypt N=16384, r=8, p=1).
#[allow(clippy::expect_used)] // SCRYPT_LOG_N/r/p/len are compile-time constants, so Params::new and scrypt() cannot Err.
pub fn derive_key(password: &str, salt: &str) -> Key {
    use scrypt::scrypt;
    let params = scrypt::Params::new(SCRYPT_LOG_N, 8, 1, 32).expect("fixed valid params");
    let mut key = [0u8; 32];
    scrypt(password.as_bytes(), salt.as_bytes(), &params, &mut key)
        .expect("fixed valid output length");
    Key::from(key)
}

/// Ciphertext size of a container holding `plain` plaintext bytes.
pub const fn encrypted_size(plain: u64) -> u64 {
    let blocks = plain / BLOCK_DATA as u64;
    let residue = plain % BLOCK_DATA as u64;
    let mut total = HEADER_SIZE + blocks * BLOCK_SIZE as u64;
    if residue != 0 {
        total += BLOCK_TAG as u64 + residue;
    }
    total
}

/// Plaintext size of a container with `ct` ciphertext bytes. `None` when
/// the size cannot describe a valid container.
#[allow(dead_code)]
pub fn decrypted_size(ct: u64) -> Option<u64> {
    let body = ct.checked_sub(HEADER_SIZE)?;
    let blocks = body / BLOCK_SIZE as u64;
    let residue = body % BLOCK_SIZE as u64;
    let mut plain = blocks * BLOCK_DATA as u64;
    if residue != 0 {
        plain += residue.checked_sub(BLOCK_TAG as u64)?;
    }
    Some(plain)
}

/// Increments a nonce little-endian with carry, exactly like the Go
/// original's `nonce.increment`.
fn increment(nonce: &mut Nonce) {
    for byte in nonce.as_mut_slice().iter_mut() {
        let (next, overflow) = byte.overflowing_add(1);
        *byte = next;
        if !overflow {
            return;
        }
    }
}

/// `nonce + blocks` over the first 8 bytes with carry, like the
/// original's `nonce.add` — used to jump to an arbitrary block for
/// range requests.
fn nonce_at(base: &[u8; NONCE_SIZE], blocks: u64) -> Nonce {
    let mut nonce = Nonce::from(*base);
    let mut carry = 0u16;
    let mut x = blocks;
    for byte in &mut nonce.as_mut_slice()[..8] {
        let xd = x as u8;
        x >>= 8;
        carry += u16::from(*byte) + u16::from(xd);
        *byte = carry as u8;
        carry >>= 8;
    }
    if carry != 0 {
        increment(&mut nonce);
    }
    nonce
}

fn random_nonce() -> [u8; NONCE_SIZE] {
    use rand::Rng;
    let mut nonce = [0u8; NONCE_SIZE];
    rand::rng().fill_bytes(&mut nonce);
    nonce
}

/// Superseded in production by the async [`EncryptingReader`], which the
/// upload path uses; this sync type remains as the tested reference
/// implementation of the block-sealing math.
#[allow(dead_code)]
pub struct Encryptor {
    cipher: XSalsa20Poly1305,
    nonce: Nonce,
    base: [u8; NONCE_SIZE],
    buf: Vec<u8>,
    started: bool,
    done: bool,
}

#[allow(dead_code)]
impl Encryptor {
    /// Creates an encryptor under `key` with a fresh random nonce.
    pub fn new(key: &Key) -> (Self, [u8; NONCE_SIZE]) {
        let base = random_nonce();
        (
            Self {
                cipher: XSalsa20Poly1305::new(key),
                nonce: Nonce::from(base),
                base,
                buf: Vec::with_capacity(BLOCK_DATA),
                started: false,
                done: false,
            },
            base,
        )
    }

    /// Encrypts `plain`, appending container bytes to `out`.
    pub fn push(&mut self, plain: &[u8], out: &mut Vec<u8>) {
        assert!(!self.done, "encryptor already finished");
        if !self.started {
            out.extend_from_slice(MAGIC);
            out.extend_from_slice(&self.base);
            self.started = true;
        }
        let mut off = 0;
        while off < plain.len() {
            let take = (plain.len() - off).min(BLOCK_DATA - self.buf.len());
            self.buf.extend_from_slice(&plain[off..off + take]);
            off += take;
            if self.buf.len() == BLOCK_DATA {
                self.seal_block();
                out.extend_from_slice(&self.buf);
                self.buf.clear();
            }
        }
    }

    /// Seals the trailing partial block, if any. Call exactly once; a
    /// container with no `push` at all is header-only (empty file).
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.done {
            return;
        }
        self.done = true;
        if !self.started {
            out.extend_from_slice(MAGIC);
            out.extend_from_slice(&self.base);
            return;
        }
        if !self.buf.is_empty() {
            self.seal_block();
            out.extend_from_slice(&self.buf);
        }
    }

    fn seal_block(&mut self) {
        let tag = self
            .cipher
            .encrypt_in_place_detached(&self.nonce, b"", &mut self.buf)
            .expect("secretbox cannot fail with a valid nonce");
        let mut sealed = tag.to_vec();
        sealed.extend_from_slice(&self.buf);
        self.buf = sealed;
        increment(&mut self.nonce);
    }
}

/// Decrypts a container stream. Two modes:
/// - [`from_header`](Self::from_header): reads the magic + nonce from the
///   stream itself (whole-container downloads)
/// - [`at_block`](Self::at_block): resumes mid-container with the nonce
///   from the database, skipping `blocks` full blocks and then `skip`
///   plaintext bytes — the range-request path
#[allow(dead_code)]
pub struct Decryptor<S: io::Read> {
    inner: S,
    cipher: XSalsa20Poly1305,
    nonce: Nonce,
    buf: Vec<u8>,
    pos: usize,
    skip: u64,
    eof: bool,
}

#[allow(dead_code)]
impl<S: io::Read> Decryptor<S> {
    /// Opens a container from its start, verifying the magic.
    pub fn from_header(mut inner: S, key: &Key) -> io::Result<Self> {
        let mut header = [0u8; HEADER_SIZE as usize];
        let n = read_exact_or_eof(&mut inner, &mut header)?;
        if n < header.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file is too short to be encrypted",
            ));
        }
        if &header[..MAGIC.len()] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an encrypted container — bad magic",
            ));
        }
        let base: [u8; NONCE_SIZE] = header[MAGIC.len()..].try_into().expect("24 bytes");
        Ok(Self::at_block(inner, key, base, 0, 0))
    }

    /// Resumes mid-container: `blocks` full blocks skipped, then `skip`
    /// plaintext bytes. The nonce must come from the database — the
    /// stream starts inside the first needed block.
    pub fn at_block(inner: S, key: &Key, base: [u8; NONCE_SIZE], blocks: u64, skip: u64) -> Self {
        Self {
            inner,
            cipher: XSalsa20Poly1305::new(key),
            nonce: nonce_at(&base, blocks),
            buf: Vec::new(),
            pos: 0,
            skip,
            eof: false,
        }
    }

    /// Pulls and decrypts the next block into `self.buf`.
    fn fill(&mut self) -> io::Result<()> {
        if self.pos < self.buf.len() || self.eof {
            return Ok(());
        }
        // Tag first, then up to BLOCK_DATA bytes; a short data read marks
        // the final block of the container.
        let mut tag = [0u8; BLOCK_TAG];
        let tn = read_exact_or_eof(&mut self.inner, &mut tag)?;
        if tn == 0 {
            self.eof = true;
            return Ok(());
        }
        if tn < BLOCK_TAG {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "container has a truncated block tag",
            ));
        }
        let mut data = vec![0u8; BLOCK_DATA];
        let dn = read_exact_or_eof(&mut self.inner, &mut data)?;
        if dn == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "container has a tag with no data",
            ));
        }
        data.truncate(dn);
        let nonce = self.nonce;
        let tag = crypto_secretbox::Tag::from(tag);
        if self
            .cipher
            .decrypt_in_place_detached(&nonce, b"", &mut data, &tag)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decryption failed — wrong key or corrupt data",
            ));
        }
        increment(&mut self.nonce);
        self.buf = data;
        self.pos = 0;
        if dn < BLOCK_DATA {
            self.eof = true;
        }
        Ok(())
    }
}

#[allow(dead_code)]
impl<S: io::Read> io::Read for Decryptor<S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // Discard the leading region first, reading and dropping plaintext
        // through a helper that does not re-enter this skip branch — the
        // prior recursive call on `self` recursed forever whenever skip
        // stayed positive across the nested read.
        if self.skip > 0 {
            let mut sink = [0u8; 8192];
            while self.skip > 0 {
                let want = self.skip.min(sink.len() as u64) as usize;
                let n = self.read_plain(&mut sink[..want])?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "container ended inside the skipped region",
                    ));
                }
                self.skip -= n as u64;
            }
        }
        self.fill()?;
        if self.pos >= self.buf.len() {
            return Ok(0); // clean EOF
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[allow(dead_code)]
impl<S: io::Read> Decryptor<S> {
    /// Reads plaintext without consulting the skip cursor; the public
    /// `io::Read` forwarding uses this for its own discard so a nested
    /// read can never loop back into skip handling.
    fn read_plain(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.fill()?;
        if self.pos >= self.buf.len() {
            return Ok(0);
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Async encrypting wrapper: reads plaintext from `inner` and exposes a
/// ciphertext `AsyncRead` stream (one self-contained container). The per-
/// part nonce this container used is recorded for later decryption.
pub struct EncryptingReader<R> {
    inner: R,
    cipher: XSalsa20Poly1305,
    nonce: Nonce,
    buf: Vec<u8>,     // encrypted bytes ready to serve
    pend: Vec<u8>,    // plaintext waiting to be sealed
    finished: bool,
}

impl<R: tokio::io::AsyncRead + Unpin> EncryptingReader<R> {
    pub fn new(inner: R, key: &Key) -> (Self, [u8; NONCE_SIZE]) {
        let base = random_nonce();
        let cipher = XSalsa20Poly1305::new(key);
        let mut buf = Vec::with_capacity(BLOCK_SIZE + 64);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&base);
        (
            Self {
                inner,
                cipher,
                nonce: Nonce::from(base),
                buf,
                pend: Vec::with_capacity(BLOCK_DATA),
                finished: false,
            },
            base,
        )
    }

    fn seal_pending_block(&mut self) {
        let tag = self
            .cipher
            .encrypt_in_place_detached(&self.nonce, b"", &mut self.pend)
            .expect("secretbox cannot fail");
        self.buf.extend_from_slice(&tag);
        self.buf.extend_from_slice(&self.pend);
        self.pend.clear();
        increment(&mut self.nonce);
    }
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for EncryptingReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        use std::task::Poll;

        // Serve buffered ciphertext first.
        if !self.buf.is_empty() {
            let n = self.buf.len().min(out.remaining());
            out.put_slice(&self.buf[..n]);
            self.buf.drain(..n);
            return Poll::Ready(Ok(()));
        }

        loop {
            // Refill a plaintext read from the inner stream.
            let mut pb = vec![0u8; BLOCK_DATA];
            let mut rbuf = tokio::io::ReadBuf::new(&mut pb);
            match Pin::new(&mut self.inner).poll_read(cx, &mut rbuf) {
                Poll::Ready(Ok(())) => {
                    let n = rbuf.filled().len();
                    if n == 0 {
                        // Inner EOF: seal the tail once and expose it.
                        if !self.finished {
                            self.finished = true;
                            if !self.pend.is_empty() {
                                self.seal_pending_block();
                            }
                        }
                        if !self.buf.is_empty() {
                            let k = self.buf.len().min(out.remaining());
                            out.put_slice(&self.buf[..k]);
                            self.buf.drain(..k);
                            return Poll::Ready(Ok(()));
                        }
                        return Poll::Ready(Ok(())); // final EOF
                    }
                    self.pend.extend_from_slice(&pb[..n]);
                    // Seal each full block immediately so large reads do
                    // not accumulate plaintext in memory.
                    while self.pend.len() >= BLOCK_DATA {
                        self.seal_pending_block();
                    }
                    if !self.buf.is_empty() {
                        let k = self.buf.len().min(out.remaining());
                        out.put_slice(&self.buf[..k]);
                        self.buf.drain(..k);
                        return Poll::Ready(Ok(()));
                    }
                    // Out completely filled of ciphertext smaller than
                    // block; loop to read more plaintext.
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Async decrypting stream: consumes ciphertext `Bytes` chunks (as served
/// by the Telegram download path) and yields decrypted plaintext chunks.
/// Supports resuming mid-container via `at_block` (for range requests):
/// `blocks` full blocks are skipped and `skip` plaintext bytes are then
/// discarded; the ciphertext stream must be pre-positioned at the first
/// needed byte (the caller slices the per-part stream accordingly).
pub struct DecryptingStream<S> {
    inner: S,
    cipher: XSalsa20Poly1305,
    /// None until the container header (magic + nonce) has been parsed.
    nonce: Option<Nonce>,
    /// Ciphertext bytes received but not yet formed into a decrypted block.
    pending: Vec<u8>,
    /// Decrypted plaintext ready to emit.
    plain: Vec<u8>,
    /// Leading plaintext bytes to discard (range sub-block remainder).
    skip: u64,
    /// Inner stream has ended (None seen).
    eof: bool,
    /// Header parsed (or `at_block` mode, which needs none).
    header_done: bool,
}

impl<S> DecryptingStream<S>
where
    S: futures::Stream<Item = io::Result<bytes::Bytes>>,
{
    /// Decrypts a whole container from its start; the first 34 bytes are
    /// the magic + nonce header.
    pub fn from_header(inner: S, key: &Key) -> Self {
        Self {
            inner,
            cipher: XSalsa20Poly1305::new(key),
            nonce: None,
            pending: Vec::new(),
            plain: Vec::new(),
            skip: 0,
            eof: false,
            header_done: false,
        }
    }

    /// Resumes mid-container: `skip` leading plaintext bytes are dropped
    /// after `blocks` full blocks. `nonce` is the container's stored nonce;
    /// the inner stream must already be positioned at the first needed
    /// byte (the caller slices the per-part stream past the header and the
    /// skipped blocks).
    pub fn at_block(
        inner: S,
        key: &Key,
        nonce: [u8; NONCE_SIZE],
        blocks: u64,
        skip: u64,
    ) -> Self {
        Self {
            inner,
            cipher: XSalsa20Poly1305::new(key),
            nonce: Some(nonce_at(&nonce, blocks)),
            pending: Vec::new(),
            plain: Vec::new(),
            skip,
            eof: false,
            header_done: true,
        }
    }

    /// Steals one complete block from `self.pending` and decrypts it,
    /// returning its plaintext. Returns `None` when the block is not yet
    /// complete (the caller decides whether EOF turns this into the final
    /// short block).
    fn try_decrypt_one(&mut self) -> io::Result<Option<Vec<u8>>> {
        let full = BLOCK_TAG + BLOCK_DATA;
        if self.pending.len() < BLOCK_TAG {
            return Ok(None);
        }
        if !self.eof && self.pending.len() < full {
            // Not EOF: need the whole block before we can assume the data
            // is complete. Wait for more ciphertext.
            return Ok(None);
        }
        // A block's plaintext is at most BLOCK_DATA bytes; when several
        // complete blocks are buffered we must decrypt exactly one per
        // call. At EOF, the final block may be short.
        let data_len = if self.eof {
            (self.pending.len() - BLOCK_TAG).min(BLOCK_DATA)
        } else {
            BLOCK_DATA
        };
        let nonce = *self.nonce.as_ref().expect("nonce parsed");
        let mut plain = self.pending[BLOCK_TAG..BLOCK_TAG + data_len].to_vec();
        let n = plain.len();
        let tag = crypto_secretbox::Tag::from_slice(&self.pending[..BLOCK_TAG]);
        if self
            .cipher
            .decrypt_in_place_detached(&nonce, b"", &mut plain, tag)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decryption failed — wrong key or corrupt data",
            ));
        }
        increment(self.nonce.as_mut().expect("nonce parsed"));
        self.pending.drain(..BLOCK_TAG + n);
        if n < BLOCK_DATA {
            self.eof = true;
        }
        Ok(Some(plain))
    }

    /// Parses the container header once 34 bytes have accumulated.
    fn parse_header(&mut self) -> io::Result<()> {
        if self.pending.len() < HEADER_SIZE as usize {
            return Ok(());
        }
        let header: Vec<u8> = self.pending.drain(..HEADER_SIZE as usize).collect();
        if &header[..MAGIC.len()] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an encrypted container — bad magic",
            ));
        }
        let base: [u8; NONCE_SIZE] = header[MAGIC.len()..].try_into().expect("24 bytes");
        self.nonce = Some(Nonce::from(base));
        self.header_done = true;
        Ok(())
    }

    fn absorb_plain(&mut self, mut plain: Vec<u8>) {
        if self.skip > 0 {
            if (self.skip as usize) >= plain.len() {
                self.skip -= plain.len() as u64;
                return;
            }
            plain.drain(..self.skip as usize);
            self.skip = 0;
        }
        self.plain.extend_from_slice(&plain);
    }
}

impl<S> futures::Stream for DecryptingStream<S>
where
    S: futures::Stream<Item = io::Result<bytes::Bytes>> + Unpin,
{
    type Item = io::Result<bytes::Bytes>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        if !self.plain.is_empty() {
            let n = self.plain.len().min(64 * 1024);
            let chunk = self.plain.drain(..n).collect::<Vec<u8>>().into();
            return Poll::Ready(Some(Ok(chunk)));
        }

        // Phase 1: pull ciphertext from the inner stream until it reports
        // Pending (nothing ready now) or ends. This is critical: a finite
        loop {
            match futures::Stream::poll_next(Pin::new(&mut self.inner), cx) {
                Poll::Ready(Some(Ok(chunk))) => self.pending.extend_from_slice(chunk.as_ref()),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    self.eof = true;
                    break;
                }
                Poll::Pending => break,
            }
        }

        // Phase 2: once the header is available, parse it.
        if !self.header_done && self.pending.len() >= HEADER_SIZE as usize {
            self.parse_header()?;
        }

        // Phase 3: decrypt whatever is now decryptable.
        // - If we have the tail and EOF, the remaining bytes form the final
        //   short block.
        // - Otherwise, a full block's worth of bytes decrypts now.
        loop {
            if let Some(plain) = self.try_decrypt_one()? {
                self.absorb_plain(plain);
                if !self.plain.is_empty() {
                    let n = self.plain.len().min(64 * 1024);
                    let chunk = self.plain.drain(..n).collect::<Vec<u8>>().into();
                    return Poll::Ready(Some(Ok(chunk)));
                }
                continue;
            }
            break;
        }

        // Nothing decryptable yet.
        if self.eof {
            // EOF and no more plaintext (a header-only or block-aligned
            // container).
            return Poll::Ready(None);
        }
        // Wait for more ciphertext; the inner poll registered a waker.
        Poll::Pending
    }
}

/// Stream item type used across download (re-exported by caller).
#[allow(dead_code)]
pub type CryptResult = io::Result<bytes::Bytes>;


/// Like `read_exact`, but a short read at true EOF is fine — returns the
/// bytes read (0 only when EOF hit before anything arrived).
#[allow(dead_code)]
fn read_exact_or_eof<R: io::Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tokio::io::AsyncReadExt;

    fn key() -> Key {
        derive_key("test-password", "test-salt")
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = key();
        let plaintext: Vec<u8> = (0..250_000u32).map(|i| (i % 251) as u8).collect();
        let (mut enc, nonce) = Encryptor::new(&key);
        let mut ct = Vec::new();
        // Feed in awkward chunk sizes to exercise the buffering.
        for chunk in plaintext.chunks(30_007) {
            enc.push(chunk, &mut ct);
        }
        enc.finish(&mut ct);

        assert_eq!(ct.len() as u64, encrypted_size(plaintext.len() as u64));
        assert_eq!(&ct[..10], b"TELDRIVE\x00\x00");
        assert_eq!(&ct[10..34], &nonce);

        let mut dec = Decryptor::from_header(ct.as_slice(), &key).unwrap();
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        assert_eq!(out, plaintext);
    }

    #[test]
    fn empty_file_is_header_only() {
        let key = key();
        let (mut enc, _n) = Encryptor::new(&key);
        let mut ct = Vec::new();
        enc.finish(&mut ct);
        assert_eq!(ct.len() as u64, HEADER_SIZE);
        let mut dec = Decryptor::from_header(ct.as_slice(), &key).unwrap();
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn range_seek_decrypts_same_bytes() {
        let key = key();
        let plaintext: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
        let (mut enc, nonce) = Encryptor::new(&key);
        let mut ct = Vec::new();
        enc.push(&plaintext, &mut ct);
        enc.finish(&mut ct);

        // Resume mid-container at a block offset + byte offset. The
        // stream must be positioned at the first needed block: bytes come
        // from the database as the raw container ciphertext, so the reader
        // resolves to this slice. `BLOCK_DATA`/`BLOCK_SIZE` are private;
        // this test knows the scheme, so it re-derives the footprint.
        let want_start = 70_000usize;
        let blocks = (want_start / BLOCK_DATA) as u64;
        let skip = (want_start % BLOCK_DATA) as u64;
        const TAG: usize = 16;
        const BLK: usize = 64 * 1024;
        let ct_block = HEADER_SIZE as usize + blocks as usize * (TAG + BLK);
        let mut dec = Decryptor::at_block(&ct[ct_block..], &key, nonce, blocks, skip);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), plaintext.len() - want_start);
        assert_eq!(&out[..100], &plaintext[want_start..want_start + 100]);
    }

    #[test]
    fn wrong_key_fails() {
        let key = key();
        let other = derive_key("other", "salt");
        let (mut enc, _n) = Encryptor::new(&key);
        let mut ct = Vec::new();
        enc.push(b"secret payload", &mut ct);
        enc.finish(&mut ct);
        let err = Decryptor::from_header(ct.as_slice(), &other)
            .unwrap()
            .read_to_end(&mut Vec::new())
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn sizes_correspond() {
        for n in [0u64, 1, 64 * 1024 - 1, 64 * 1024, 64 * 1024 + 1, 3 * 64 * 1024] {
            let c = encrypted_size(n);
            assert_eq!(decrypted_size(c), Some(n), "n={n}");
        }
    }
    #[test]
    fn nonce_b64_roundtrips_and_rejects_garbage() {
        let n = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23];
        let s = nonce_b64(&n);
        assert_eq!(nonce_from_b64(&s), Some(n));
        // Wrong length decodes but fails to fit a nonce; junk fails to
        // decode. Either way a corrupt stored nonce reads as plaintext.
        assert_eq!(nonce_from_b64("not-base64!!"), None);
        assert_eq!(nonce_from_b64(&base64_engine_encode(&[0u8; 10])), None);
    }

    fn base64_engine_encode(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[tokio::test]
    async fn encrypting_reader_to_decrypting_stream_roundtrip() {
        use bytes::Bytes;
        use futures::StreamExt;

        let key = key();
        let plaintext: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();

        // Encrypt via the async reader.
        let (mut er, _nonce) = EncryptingReader::new(plaintext.as_slice(), &key);
        let mut ct = Vec::new();
        er.read_to_end(&mut ct).await.unwrap();
        assert_eq!(ct.len() as u64, encrypted_size(plaintext.len() as u64));

        // Decrypt via the async stream, fed in awkward chunk sizes.
        let chunks: Vec<io::Result<Bytes>> = ct
            .chunks(31_999)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let src = futures::stream::iter(chunks);
        let mut dec = DecryptingStream::from_header(src, &key);
        let mut out = Vec::new();
        while let Some(item) = dec.next().await {
            let b = item.unwrap();
            out.extend_from_slice(&b);
        }
        assert_eq!(out.len(), plaintext.len());
        assert_eq!(out, plaintext);
    }

    #[tokio::test]
    async fn decrypting_stream_range_via_at_block() {
        use bytes::Bytes;
        use futures::StreamExt;

        let key = key();
        let plaintext: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
        let (mut er, nonce) = EncryptingReader::new(plaintext.as_slice(), &key);
        let mut ct = Vec::new();
        er.read_to_end(&mut ct).await.unwrap();

        // Range request: start at byte 70_000. Skip full blocks then the
        // intra-block remainder.
        let want = 70_000usize;
        let blocks = (want / BLOCK_DATA) as u64;
        let skip = (want % BLOCK_DATA) as u64;
        let block_ct = (HEADER_SIZE as usize) + blocks as usize * BLOCK_SIZE as usize;
        let src = futures::stream::iter(
            ct[block_ct..]
                .chunks(40_003)
                .map(|c| Ok(Bytes::copy_from_slice(c))),
        );
        let mut dec = DecryptingStream::at_block(src, &key, nonce, blocks, skip);
        let mut out = Vec::new();
        while let Some(item) = dec.next().await {
            out.extend_from_slice(&item.unwrap());
        }
        assert_eq!(out.len(), plaintext.len() - want);
        assert_eq!(out, plaintext[want..].to_vec());
    }

    #[tokio::test]
    async fn decrypting_stream_rejects_wrong_key() {
        use bytes::Bytes;
        use futures::StreamExt;

        let key = key();
        let other = derive_key("other", "salt");
        let plaintext: Vec<u8> = b"secret container".to_vec();
        let (mut er, _n) = EncryptingReader::new(plaintext.as_slice(), &key);
        let mut ct = Vec::new();
        er.read_to_end(&mut ct).await.unwrap();
        let src = futures::stream::iter(vec![Ok::<_, io::Error>(Bytes::copy_from_slice(&ct))]);
        let mut dec = DecryptingStream::from_header(src, &other);
        let first = dec.next().await.expect("a chunk");
        assert!(first.is_err());
    }


}
