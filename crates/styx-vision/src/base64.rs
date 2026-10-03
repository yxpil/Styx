//! 极小 base64 编码。
//!
//! 只做编码（把图片塞进 JSON），不做解码——这条链路上没有任何地方需要解码。
//! 一个 25 行的实现换来 `styx-vision` 少一个第三方依赖，值得。

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 标准 base64 编码（带 `=` 填充）。
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// 带 MIME 前缀的 data URL。
pub fn data_url(mime: &str, data: &[u8]) -> String {
    format!("data:{mime};base64,{}", encode(data))
}

/// 按魔数猜 MIME。
pub fn mime_of(data: &[u8]) -> &'static str {
    match crate::decode::sniff(data) {
        Some(crate::decode::Format::Png) => "image/png",
        Some(crate::decode::Format::Bmp) => "image/bmp",
        Some(crate::decode::Format::Jpeg) => "image/jpeg",
        None => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_rfc4648_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn handles_high_bytes() {
        assert_eq!(encode(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(encode(&[0x00, 0x00, 0x00]), "AAAA");
        assert_eq!(encode(&[0xfb, 0xef, 0xbe]), "++++");
    }

    #[test]
    fn output_is_always_padded_to_a_multiple_of_four() {
        for n in 0..40usize {
            let data = vec![0xa5u8; n];
            let out = encode(&data);
            assert_eq!(out.len() % 4, 0, "n={n}");
            assert_eq!(out.len(), n.div_ceil(3) * 4, "n={n}");
        }
    }

    #[test]
    fn data_url_and_mime_detection() {
        let png = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        assert_eq!(mime_of(&png), "image/png");
        assert_eq!(mime_of(&[0xff, 0xd8, 0xff]), "image/jpeg");
        assert_eq!(mime_of(b"junk"), "application/octet-stream");
        assert!(data_url("image/png", &png).starts_with("data:image/png;base64,"));
    }
}
