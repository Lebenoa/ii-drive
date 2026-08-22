//! Embedded cover-art extraction for audio containers: ID3v2 `APIC`
//! frames (mp3 and friends) and FLAC `PICTURE` metadata blocks. Pure
//! byte parsing, no decode — the payload is already a jpeg/png.

fn be32(b: &[u8]) -> usize {
    ((b[0] as usize) << 24) | ((b[1] as usize) << 16) | ((b[2] as usize) << 8) | b[3] as usize
}

/// Syncsafe integer (ID3v2.4 / tag size): 7 bits per byte.
fn syncsafe(b: &[u8]) -> usize {
    ((b[0] as usize) & 0x7f) << 21
        | ((b[1] as usize) & 0x7f) << 14
        | ((b[2] as usize) & 0x7f) << 7
        | ((b[3] as usize) & 0x7f)
}

/// Returns the embedded image bytes when the buffer holds recognizable art.
pub fn extract(data: &[u8]) -> Option<Vec<u8>> {
    id3_apic(data).or_else(|| flac_picture(data))
}

/// Sanity check that the payload starts like a real image.
fn looks_like_image(b: &[u8]) -> bool {
    b.starts_with(&[0xff, 0xd8]) || b.starts_with(&[0x89, b'P', b'N', b'G']) || b.starts_with(b"GIF")
}

fn id3_apic(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 10 || &data[..3] != b"ID3" {
        return None;
    }
    let version = data[3];
    let tag_size = syncsafe(&data[6..10]);
    let body = data.get(10..10 + tag_size.min(data.len().saturating_sub(10)))?;

    let mut off = 0usize;
    while off + 10 <= body.len() {
        let id = &body[off..off + 4];
        if id[0] == 0 {
            break; // padding
        }
        // v2.4 frame sizes are syncsafe; v2.2/2.3 are plain big-endian.
        let size = if version >= 4 {
            syncsafe(&body[off + 4..off + 8])
        } else {
            be32(&body[off + 4..off + 8])
        };
        let frame = body.get(off + 10..off + 10 + size)?;
        if id == b"APIC"
            && frame.len() > 4
            && let Some(img) = apic_payload(frame)
        {
            return Some(img);
        }
        off += 10 + size;
    }
    None
}

/// APIC layout: text-encoding byte, latin1 mime + NUL, picture type,
/// description + terminator (1 or 2 NULs depending on encoding), image.
fn apic_payload(frame: &[u8]) -> Option<Vec<u8>> {
    let enc = frame[0];
    let mime_end = frame[1..].iter().position(|&b| b == 0)? + 1;
    let mime = &frame[1..1 + mime_end - 1];
    if !mime.is_empty() && !mime.eq_ignore_ascii_case(b"image/jpeg") && !mime.eq_ignore_ascii_case(b"image/png") {
        // Unknown mime: still try — the bytes decide below.
    }
    let mut p = 1 + mime_end + 1; // skip encoding, mime+NUL, picture type
    if p >= frame.len() {
        return None;
    }
    // Description terminator: encoding 1/2 = UTF-16 (00 00), else latin1 (00).
    let term_len = if enc == 1 || enc == 2 { 2 } else { 1 };
    if term_len == 2 {
        while p + 1 < frame.len() && !(frame[p] == 0 && frame[p + 1] == 0) {
            p += 2;
        }
        p += 2;
    } else {
        while p < frame.len() && frame[p] != 0 {
            p += 1;
        }
        p += 1;
    }
    let img = frame.get(p..)?;
    if looks_like_image(img) {
        Some(img.to_vec())
    } else {
        None
    }
}

fn flac_picture(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 8 || &data[..4] != b"fLaC" {
        return None;
    }
    let mut off = 4usize;
    loop {
        let head = data.get(off..off + 4)?;
        let block_type = head[0] & 0x7f;
        let size = (head[1] as usize) << 16 | (head[2] as usize) << 8 | head[3] as usize;
        let body = data.get(off + 4..off + 4 + size)?;
        if block_type == 6 {
            // type(4) mime_len(4) mime desc_len(4) desc w/h/depth/colors(16) data_len(4)
            let mut p = 4usize;
            let mime_len = be32(body.get(p..p + 4)?);
            p += 4 + mime_len;
            let desc_len = be32(body.get(p..p + 4)?);
            p += 4 + desc_len + 16;
            let data_len = be32(body.get(p..p + 4)?);
            p += 4;
            let img = body.get(p..p + data_len)?;
            if looks_like_image(img) {
                return Some(img.to_vec());
            }
            return None;
        }
        if head[0] & 0x80 != 0 {
            return None; // last block, no picture seen
        }
        off += 4 + size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apic_frame(enc: u8, mime: &[u8], desc: &[u8], img: &[u8]) -> Vec<u8> {
        let mut f = vec![enc];
        f.extend_from_slice(mime);
        f.push(0);
        f.push(3); // front cover
        f.extend_from_slice(desc);
        f.push(0);
        if enc == 1 {
            f.push(0);
        }
        f.extend_from_slice(img);
        let mut frame = vec![b'A', b'P', b'I', b'C', 0, 0, 0, 0, 0, 0];
        let n = f.len() as u32;
        let be = n.to_be_bytes();
        frame[4..8].copy_from_slice(&be);
        frame.extend_from_slice(&f);
        frame
    }

    fn id3(frames: &[u8]) -> Vec<u8> {
        let mut out = vec![b'I', b'D', b'3', 3, 0, 0];
        let sz = syncsafe(&[0, (frames.len() >> 14) as u8, (frames.len() >> 7) as u8, frames.len() as u8]);
        assert_eq!(sz, frames.len(), "test frame must fit syncsafe");
        out.extend_from_slice(&[0, (frames.len() >> 14) as u8 & 0x7f, (frames.len() >> 7) as u8 & 0x7f, frames.len() as u8 & 0x7f]);
        out.extend_from_slice(frames);
        out
    }

    #[test]
    fn mp3_id3_cover() {
        let jpeg: Vec<u8> = [0xffu8, 0xd8].iter().copied().chain([0x11; 40]).collect();
        let mut frames = apic_frame(0, b"image/jpeg", b"cover", &jpeg);
        // second frame (TIT2) after APIC to exercise iteration
        frames.extend_from_slice(&[b'T', b'I', b'T', b'2', 0, 0, 0, 3, 0, 0, 1, b'x', 0]);
        let file = id3(&frames);
        assert_eq!(extract(&file).unwrap(), jpeg);
    }

    #[test]
    fn flac_cover() {
        let png: Vec<u8> = [0x89, b'P', b'N', b'G'].iter().copied().chain([0x7; 24]).collect();
        let mut body = vec![0u8; 4]; // picture type: cover front
        body.extend_from_slice(&(9u32.to_be_bytes())); // "image/png"
        body.extend_from_slice(b"image/png");
        body.extend_from_slice(&0u32.to_be_bytes()); // empty description
        body.extend_from_slice(&[0u8; 16]); // dims/depth/colors
        body.extend_from_slice(&(png.len() as u32).to_be_bytes());
        body.extend_from_slice(&png);

        let mut file = b"fLaC".to_vec();
        file.push(0x80 | 6); // last-block flag + PICTURE
        file.extend_from_slice(&[(body.len() >> 16) as u8, (body.len() >> 8) as u8, body.len() as u8]);
        file.extend_from_slice(&body);
        assert_eq!(extract(&file).unwrap(), png);
    }

    #[test]
    fn rejects_garbage() {
        assert!(extract(b"ID3\x03\x00\x00\x00\x00\x00\x0fgarbage").is_none());
        assert!(extract(b"fLaC\x00\x00\x00\x00").is_none());
        assert!(extract(&[]).is_none());
    }
}
