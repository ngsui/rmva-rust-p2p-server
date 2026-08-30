//! net-core — 协议核心（平台无关）
//!
//! 帧格式：4 字节大端 u32 长度 + payload（UTF-8 JSON）
//! 该 crate 不含任何平台代码，PC 的 rgss3_rust_net.dll 与
//! 未来的 Android .so / Linux 服务器全部复用这里。

/// 单帧 payload 上限（4MB），防御恶意/损坏的长度头
pub const MAX_PAYLOAD: usize = 4 * 1024 * 1024;

/// 帧头长度：4 字节大端 u32
pub const HEADER_LEN: usize = 4;

/// 编码：payload -> [长度头(4字节大端)][payload]
pub fn encode(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// 解码错误
#[derive(Debug)]
pub enum DecodeError {
    /// 长度头超过 MAX_PAYLOAD，流数据已不可信，调用方应断开连接
    PayloadTooLarge(usize),
}

/// 流式解码器：TCP 是字节流，粘包/半包由它吸收
///
/// 用法：每收到一段 socket 数据就 `feed()` 一次，
/// 返回这段时间内解析出的所有完整帧。
pub struct Decoder {
    buf: Vec<u8>,
    pos: usize, // 已消费游标，避免每帧 drain 整个缓冲区
}

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            buf: Vec::with_capacity(16 * 1024),
            pos: 0,
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>, DecodeError> {
        self.buf.extend_from_slice(data);
        let mut frames = Vec::new();
        loop {
            let avail = self.buf.len() - self.pos;
            if avail < HEADER_LEN {
                break;
            }
            let len =
                u32::from_be_bytes(self.buf[self.pos..self.pos + HEADER_LEN].try_into().unwrap())
                    as usize;
            if len > MAX_PAYLOAD {
                return Err(DecodeError::PayloadTooLarge(len));
            }
            if avail < HEADER_LEN + len {
                break; // 半包，等下一次数据
            }
            let start = self.pos + HEADER_LEN;
            frames.push(self.buf[start..start + len].to_vec());
            self.pos += HEADER_LEN + len;
        }
        // 一次性回收已消费前缀，控制缓冲区不无限膨胀
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 编码后可完整解码() {
        let mut d = Decoder::new();
        let frames = d.feed(&encode(b"{\"type\":\"ping\"}")).unwrap();
        assert_eq!(frames, vec![b"{\"type\":\"ping\"}".to_vec()]);
    }

    #[test]
    fn 粘包_多条帧一次到达() {
        let mut d = Decoder::new();
        let mut raw = encode(b"aaa");
        raw.extend(encode(b"bbbb"));
        raw.extend(encode(b"c"));
        let frames = d.feed(&raw).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[1], b"bbbb".to_vec());
    }

    #[test]
    fn 半包_分两次到达() {
        let mut d = Decoder::new();
        let raw = encode(b"hello");
        // 头 2 字节 + 前 3 字节 payload 先到
        let (a, b) = raw.split_at(5);
        assert!(d.feed(a).unwrap().is_empty());
        let frames = d.feed(b).unwrap();
        assert_eq!(frames, vec![b"hello".to_vec()]);
    }

    #[test]
    fn 超长帧头被拒绝() {
        let mut d = Decoder::new();
        let mut raw = (MAX_PAYLOAD as u32 + 1).to_be_bytes().to_vec();
        raw.extend_from_slice(b"x");
        assert!(d.feed(&raw).is_err());
    }

    #[test]
    fn 空_payload_合法() {
        let mut d = Decoder::new();
        let frames = d.feed(&encode(b"")).unwrap();
        assert_eq!(frames, vec![Vec::<u8>::new()]);
    }
}
