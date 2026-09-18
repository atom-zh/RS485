//! 测试帧：魔数 A5 5A + 序号 + 长度 + payload + CRC-16/IBM。

pub const MAGIC: [u8; 2] = [0xA5, 0x5A];
pub const HEADER_LEN: usize = 8; // magic + seq + len
pub const CRC_LEN: usize = 2;
pub const OVERHEAD: usize = HEADER_LEN + CRC_LEN;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestFrame {
    pub seq: u32,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    NotTestFrame,
    BadCrc,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameView {
    Ok(TestFrame),
    NotTest,
    BadCrc,
}

/// CRC-16/IBM（ARC）：poly 0x8005 reflected 0xA001，init 0。
pub fn crc16_ibm(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

pub fn encode(seq: u32, payload: &[u8]) -> Result<Vec<u8>, &'static str> {
    if payload.len() > u16::MAX as usize {
        return Err("payload 超过 u16 长度");
    }
    let mut body = Vec::with_capacity(6 + payload.len());
    body.extend_from_slice(&seq.to_le_bytes());
    body.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    body.extend_from_slice(payload);
    let crc = crc16_ibm(&body);
    let mut out = Vec::with_capacity(MAGIC.len() + body.len() + CRC_LEN);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(out)
}

pub fn decode(data: &[u8]) -> Result<TestFrame, DecodeError> {
    if data.len() < OVERHEAD || data[0] != MAGIC[0] || data[1] != MAGIC[1] {
        return Err(DecodeError::NotTestFrame);
    }
    let seq = u32::from_le_bytes(data[2..6].try_into().unwrap());
    let len = u16::from_le_bytes(data[6..8].try_into().unwrap()) as usize;
    let need = OVERHEAD + len;
    if data.len() != need {
        return Err(DecodeError::BadCrc);
    }
    let body = &data[2..8 + len];
    let got = u16::from_le_bytes(data[8 + len..need].try_into().unwrap());
    if got != crc16_ibm(body) {
        return Err(DecodeError::BadCrc);
    }
    Ok(TestFrame {
        seq,
        payload: data[8..8 + len].to_vec(),
    })
}

/// 一段空闲组帧里可能含多帧；无魔数则整段记为非测试帧。
pub fn inspect(data: &[u8]) -> Vec<FrameView> {
    if data.len() < 2 || data[0] != MAGIC[0] || data[1] != MAGIC[1] {
        return vec![FrameView::NotTest];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if data.len() - i < OVERHEAD {
            out.push(FrameView::BadCrc);
            break;
        }
        if data[i] != MAGIC[0] || data[i + 1] != MAGIC[1] {
            out.push(FrameView::BadCrc);
            break;
        }
        let len = u16::from_le_bytes(data[i + 6..i + 8].try_into().unwrap()) as usize;
        let need = OVERHEAD + len;
        if i + need > data.len() {
            out.push(FrameView::BadCrc);
            break;
        }
        match decode(&data[i..i + need]) {
            Ok(frame) => out.push(FrameView::Ok(frame)),
            Err(DecodeError::NotTestFrame) => out.push(FrameView::NotTest),
            Err(DecodeError::BadCrc) => out.push(FrameView::BadCrc),
        }
        i += need;
    }
    out
}

#[derive(Default)]
pub struct SeqTracker {
    next: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SeqOutcome {
    pub lost: u64,
    pub dup: u64,
}

impl SeqTracker {
    pub fn observe(&mut self, seq: u32) -> SeqOutcome {
        match self.next {
            None => {
                self.next = Some(seq.wrapping_add(1));
                SeqOutcome::default()
            }
            Some(expected) if seq == expected => {
                self.next = Some(expected.wrapping_add(1));
                SeqOutcome::default()
            }
            Some(expected) if seq.wrapping_sub(expected) < u32::MAX / 2 => {
                let lost = u64::from(seq.wrapping_sub(expected));
                self.next = Some(seq.wrapping_add(1));
                SeqOutcome { lost, dup: 0 }
            }
            Some(_) => SeqOutcome { lost: 0, dup: 1 },
        }
    }
}

pub fn pattern_payload(seq: u32, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (seq as u8).wrapping_add(i as u8))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let raw = encode(7, b"hello").unwrap();
        let frame = decode(&raw).unwrap();
        assert_eq!(frame.seq, 7);
        assert_eq!(frame.payload, b"hello");
        assert!(matches!(inspect(&raw)[0], FrameView::Ok(_)));
    }

    #[test]
    fn bad_crc() {
        let mut raw = encode(1, b"x").unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        assert_eq!(decode(&raw), Err(DecodeError::BadCrc));
    }

    #[test]
    fn seq_gaps() {
        let mut t = SeqTracker::default();
        assert_eq!(t.observe(0).lost, 0);
        assert_eq!(t.observe(1).lost, 0);
        assert_eq!(t.observe(4).lost, 2);
        assert_eq!(t.observe(4).dup, 1);
    }
}
