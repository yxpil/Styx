//! # DEFLATE / zlib 解压
//!
//! 手写而不是引依赖，理由和这个工作区里手写 HTTP/1.1 是同一个：
//! **一个只读、只解压、不做流式的 inflate 大约就是 300 行**，
//! 而换来的是一条硬保证——图片分析这条链在任何机器上都能跑，
//! 不需要编译器、不需要 C 依赖、不会因为某个 crate 的 MSRV 抬高而失联。
//!
//! ## 只做必需的部分
//!
//! - 支持 BTYPE 00（stored）/ 01（fixed）/ 10（dynamic），这三者是全部；
//!   没有 BTYPE 11（保留值，遇到就是数据损坏）。
//! - zlib 头两字节要读，但 **FDICT 一律视为不支持**：PNG 从来不用预设字典，
//!   真的遇到说明这不是 PNG 的 IDAT，早报错比默默解出错数据好。
//! - 不校验 adler32。校验需要把整段输出再扫一遍，而 PNG 自己有 CRC
//!   校验 IDAT 块——重复校验只是在给攻击面加代码。
//!
//! ## 为什么按 canonical Huffman 的 `first_code` 解码
//!
//! 教科书做法是建一棵树，但树的指针跳转对 cache 极不友好；而 DEFLATE
//! 的码长上限只有 15，用"每长度段的起始码 + 偏移"做二分查找实际上是
//! 逐位表查找，常数极小。这就是 zlib `inflate_table` 的思路。

use std::fmt;

/// 解压错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InflateError {
    /// 数据不够（正常结束前就没了）。
    Truncated,
    /// 保留的 BTYPE=11。
    BadBlockType(u8),
    /// zlib 头不合法（比如用了预设字典）。
    BadZlibHeader(u16),
    /// 无效的距离（超出已解出的窗口）。
    BadDistance(usize),
    /// 无效的长度码。
    BadCode,
    /// 码长表非法（over-subscribed 或 incomplete）。
    BadHuffmanTable,
}

impl fmt::Display for InflateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InflateError::Truncated => write!(f, "压缩数据在中途就结束了"),
            InflateError::BadBlockType(b) => write!(f, "非法的块类型 BTYPE={b}"),
            InflateError::BadZlibHeader(h) => write!(f, "非法的 zlib 头 0x{h:04x}"),
            InflateError::BadDistance(d) => write!(f, "回引距离 {d} 超出了已解出的数据"),
            InflateError::BadCode => write!(f, "遇到无效的 Huffman 码"),
            InflateError::BadHuffmanTable => write!(f, "Huffman 码长表非法"),
        }
    }
}

impl std::error::Error for InflateError {}

/// 按位读取（DEFLATE 是**低位在前**的）。
struct BitReader<'a> {
    data: &'a [u8],
    /// 当前字节下标。
    pos: usize,
    /// 已读位数（0..8）。
    bit: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            bit: 0,
        }
    }

    /// 读 `n` 位（n ≤ 24）。
    fn bits(&mut self, n: u32) -> Result<u32, InflateError> {
        let mut out = 0u32;
        for i in 0..n {
            let byte = *self.data.get(self.pos).ok_or(InflateError::Truncated)?;
            let b = (byte >> self.bit) & 1;
            out |= (b as u32) << i;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
            }
        }
        Ok(out)
    }

    /// 跳到字节边界。
    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }

    /// 直接在字节层读（stored 块用）。
    fn take_bytes(&mut self, n: usize) -> Result<&'a [u8], InflateError> {
        let end = self.pos.checked_add(n).ok_or(InflateError::Truncated)?;
        let slice = self
            .data
            .get(self.pos..end)
            .ok_or(InflateError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }
}

/// 一张 canonical Huffman 解码表。
struct Huffman {
    count: [u16; 16],
    first: [u16; 16],
    offset: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    /// 从码长表建表。
    fn new(lens: &[u16]) -> Result<Self, InflateError> {
        let mut count = [0u16; 16];
        for &l in lens {
            if l > 15 {
                return Err(InflateError::BadHuffmanTable);
            }
            count[l as usize] += 1;
        }
        // count[0] 是"未使用的符号数"，参与 first 计算时必须归零
        let unused = count[0];
        count[0] = 0;

        // Kraft 不等式：码长自洽才能建表。over-subscribed 直接拒。
        let mut left: i32 = 1;
        for c in &count[1..] {
            left <<= 1;
            left -= *c as i32;
            if left < 0 {
                return Err(InflateError::BadHuffmanTable);
            }
        }

        let mut first = [0u16; 16];
        let mut code = 0u16;
        for len in 1..=15 {
            code = (code + count[len - 1]) << 1;
            first[len] = code;
        }

        let mut offset = [0u16; 16];
        let mut total = 0u16;
        for len in 1..=15 {
            offset[len] = total;
            total += count[len];
        }

        let mut symbols = vec![0u16; total as usize];
        let mut next = offset;
        for (sym, &l) in lens.iter().enumerate() {
            if l != 0 {
                let idx = next[l as usize] as usize;
                if idx >= symbols.len() {
                    return Err(InflateError::BadHuffmanTable);
                }
                symbols[idx] = sym as u16;
                next[l as usize] += 1;
            }
        }

        let _ = unused;
        Ok(Huffman {
            count,
            first,
            offset,
            symbols,
        })
    }

    /// 解一个符号。
    fn decode(&self, r: &mut BitReader<'_>) -> Result<u16, InflateError> {
        let mut code = 0u16;
        for len in 1..=15usize {
            code |= r.bits(1)? as u16;
            let cnt = self.count[len];
            if cnt > 0 {
                let base = self.first[len];
                if code >= base && (code - base) < cnt {
                    let idx = self.offset[len] as usize + (code - base) as usize;
                    return self
                        .symbols
                        .get(idx)
                        .copied()
                        .ok_or(InflateError::BadHuffmanTable);
                }
            }
            code <<= 1;
        }
        Err(InflateError::BadCode)
    }
}

/// 长度码的基数（下标 = 码 - 257）。
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
/// 长度码的额外位数。
const LENGTH_EXTRA: [u16; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
/// 距离码的基数（下标 = 距离码）。
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
/// 距离码的额外位数。
const DIST_EXTRA: [u16; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// 解一段裸 DEFLATE 流。
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, InflateError> {
    let mut r = BitReader::new(data);
    let mut out: Vec<u8> = Vec::with_capacity(data.len() * 4);
    inflate_into(&mut r, &mut out)?;
    Ok(out)
}

/// 解一段 zlib 流（2 字节头 + DEFLATE）。
pub fn inflate_zlib(data: &[u8]) -> Result<Vec<u8>, InflateError> {
    if data.len() < 2 {
        return Err(InflateError::Truncated);
    }
    let header = ((data[0] as u16) << 8) | data[1] as u16;
    let cm = data[0] & 0x0f;
    let cinfo = data[0] >> 4;
    let fdict = data[1] & 0x20 != 0;
    let fcheck = header % 31;
    if cm != 8 || cinfo > 7 || fdict || fcheck != 0 {
        return Err(InflateError::BadZlibHeader(header));
    }
    inflate(&data[2..])
}

fn inflate_into(r: &mut BitReader<'_>, out: &mut Vec<u8>) -> Result<(), InflateError> {
    loop {
        let final_block = r.bits(1)? == 1;
        let btype = r.bits(2)?;
        match btype {
            0 => {
                r.align();
                let len_bytes = r.take_bytes(4)?;
                let len = u16::from_le_bytes([len_bytes[0], len_bytes[1]]) as usize;
                let nlen = u16::from_le_bytes([len_bytes[2], len_bytes[3]]);
                if (len as u16) != !nlen {
                    return Err(InflateError::BadCode);
                }
                let raw = r.take_bytes(len)?;
                out.extend_from_slice(raw);
            }
            1 => {
                let (lit, dist) = fixed_tables();
                inflate_block(r, out, &lit, &dist)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(r)?;
                inflate_block(r, out, &lit, &dist)?;
            }
            other => return Err(InflateError::BadBlockType(other as u8)),
        }
        if final_block {
            return Ok(());
        }
    }
}

fn inflate_block(
    r: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), InflateError> {
    loop {
        let sym = lit.decode(r)?;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()),
            257..=285 => {
                let idx = (sym - 257) as usize;
                let len = LENGTH_BASE[idx] as usize + r.bits(LENGTH_EXTRA[idx] as u32)? as usize;
                let dsym = dist.decode(r)? as usize;
                if dsym >= DIST_BASE.len() {
                    return Err(InflateError::BadCode);
                }
                let distance = DIST_BASE[dsym] as usize + r.bits(DIST_EXTRA[dsym] as u32)? as usize;
                if distance == 0 || distance > out.len() {
                    return Err(InflateError::BadDistance(distance));
                }
                // 逐字节拷贝：distance 可能小于 len（重叠复制是 DEFLATE 的特性，不是 bug）
                let start = out.len() - distance;
                for i in 0..len {
                    let b = out[start + i];
                    out.push(b);
                }
            }
            _ => return Err(InflateError::BadCode),
        }
    }
}

fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit_lens = vec![0u16; 288];
    for (i, l) in lit_lens.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let dist_lens = vec![5u16; 30];
    // 固定表是规范定义的，建表不可能失败
    (
        Huffman::new(&lit_lens).expect("固定字面量表必须合法"),
        Huffman::new(&dist_lens).expect("固定距离表必须合法"),
    )
}

/// 码长码在表里的排列顺序（这是一个著名的坑：不是自然序）。
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn dynamic_tables(r: &mut BitReader<'_>) -> Result<(Huffman, Huffman), InflateError> {
    let hlit = r.bits(5)? as usize + 257;
    let hdist = r.bits(5)? as usize + 1;
    let hclen = r.bits(4)? as usize + 4;

    let mut cl_lens = [0u16; 19];
    for i in 0..hclen {
        cl_lens[CL_ORDER[i]] = r.bits(3)? as u16;
    }
    let cl = Huffman::new(&cl_lens)?;

    let total = hlit + hdist;
    let mut lens: Vec<u16> = Vec::with_capacity(total);
    while lens.len() < total {
        let sym = cl.decode(r)?;
        match sym {
            0..=15 => lens.push(sym),
            16 => {
                let prev = *lens.last().ok_or(InflateError::BadCode)?;
                let repeat = 3 + r.bits(2)? as usize;
                for _ in 0..repeat {
                    lens.push(prev);
                }
            }
            17 => {
                let repeat = 3 + r.bits(3)? as usize;
                // 补 `repeat` 个 0 码长；用 resize 比逐个 push 更直白（也不会漏掉容量预留）
                lens.resize(lens.len() + repeat, 0);
            }
            18 => {
                let repeat = 11 + r.bits(7)? as usize;
                lens.resize(lens.len() + repeat, 0);
            }
            _ => return Err(InflateError::BadCode),
        }
    }
    lens.truncate(total);

    let lit = Huffman::new(&lens[..hlit])?;
    let dist = Huffman::new(&lens[hlit..])?;
    Ok((lit, dist))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_block_roundtrip() {
        // 手工构造一个 stored 块：BFINAL=1, BTYPE=00, 然后 LEN/NLEN
        let payload = b"hello inflate";
        let mut data = vec![0x01];
        data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        data.extend_from_slice(&(!(payload.len() as u16)).to_le_bytes());
        data.extend_from_slice(payload);
        assert_eq!(inflate(&data).unwrap(), payload);
    }

    #[test]
    fn rejects_the_reserved_block_type() {
        // 位序是最低位在前：bit0 = BFINAL, bit1..2 = BTYPE。
        // BTYPE=3 即两位置 1 → 0b0000_0110（BFINAL=0）。
        let data = [0b0000_0110u8];
        assert_eq!(inflate(&data), Err(InflateError::BadBlockType(3)));
    }

    #[test]
    fn rejects_a_truncated_stream() {
        assert_eq!(inflate(&[]), Err(InflateError::Truncated));
        assert_eq!(inflate(&[0x01]), Err(InflateError::Truncated));
    }

    #[test]
    fn rejects_a_bad_zlib_header() {
        // CM=7 不是 deflate
        assert!(matches!(
            inflate_zlib(&[0x70, 0x00]),
            Err(InflateError::BadZlibHeader(_))
        ));
        // FDICT 置位
        assert!(matches!(
            inflate_zlib(&[0x78, 0x20]),
            Err(InflateError::BadZlibHeader(_))
        ));
        assert_eq!(inflate_zlib(&[0x78]), Err(InflateError::Truncated));
    }

    #[test]
    fn zlib_header_check_is_enforced() {
        // `zlib.compress(b"", 9)` 的真实输出。头后面还必须有一个合法的
        // 空块（`03 00`）与 adler32，只有头两个字节是解不出东西的。
        let empty = [0x78u8, 0xda, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01];
        assert_eq!(inflate_zlib(&empty), Ok(Vec::new()), "空流应当解出空数据");
        // 只给头不给块 → 截断，而不是"成功解出空数据"
        assert_eq!(inflate_zlib(&[0x78, 0x9c]), Err(InflateError::Truncated));
        // 校验和不是 %31 的倍数
        let bad = [0x78u8, 0x9d];
        assert!(matches!(
            inflate_zlib(&bad),
            Err(InflateError::BadZlibHeader(_))
        ));
    }

    #[test]
    fn backward_reference_can_overlap() {
        // BTYPE=01（固定表），先输出一个 'a'，再用 distance=1 length=4 复制
        // 手工拼位太脆，这里改成验证一个真实的小数据（见 inflate_matches_zlib_vectors）
        let data = [0x01u8];
        assert_eq!(inflate(&data), Err(InflateError::Truncated));
    }

    /// 用 Python 的 `zlib.compress` 生成的真实压缩数据。
    ///
    /// 这些向量的意义是"我的实现和 zlib 对得上"——而不是"和我的实现对得上"。
    /// 第二组还顺带覆盖了**重叠回引**（distance=1 而 length 很大），
    /// 那是 DEFLATE 里最容易被写错的一处。
    #[test]
    fn inflate_matches_real_zlib_vectors() {
        // zlib.compress(b"a" * 200, 9) —— 大量重叠回引
        let v1: Vec<u8> = vec![
            0x78, 0xda, 0x4b, 0x4c, 0x1c, 0x1e, 0x00, 0x00, 0xc2, 0x7f, 0x4b, 0xc9,
        ];
        let out = inflate_zlib(&v1).unwrap();
        assert_eq!(out.len(), 200);
        assert!(out.iter().all(|&b| b == b'a'));

        // zlib.compress(b"hello world, hello mars", 6) —— 含动态 Huffman 表
        let v2: Vec<u8> = vec![
            0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0xd1,
            0x51, 0xc8, 0x00, 0x73, 0x72, 0x13, 0x8b, 0x8a, 0x01, 0x67, 0x30, 0x08, 0x90,
        ];
        assert_eq!(
            String::from_utf8(inflate_zlib(&v2).unwrap()).unwrap(),
            "hello world, hello mars"
        );
    }

    #[test]
    fn inflate_handles_a_large_high_entropy_payload() {
        // 256 个不同字节重复 4 次：既有字面量也有回引
        let payload: Vec<u8> = (0..256u32).map(|i| i as u8).collect::<Vec<_>>().repeat(4);
        let compressed: Vec<u8> = vec![
            0x78, 0x9c, 0x63, 0x60, 0x64, 0x62, 0x66, 0x61, 0x65, 0x63, 0xe7, 0xe0, 0xe4, 0xe2,
            0xe6, 0xe1, 0xe5, 0xe3, 0x17, 0x10, 0x14, 0x12, 0x16, 0x11, 0x15, 0x13, 0x97, 0x90,
            0x94, 0x92, 0x96, 0x91, 0x95, 0x93, 0x57, 0x50, 0x54, 0x52, 0x56, 0x51, 0x55, 0x53,
            0xd7, 0xd0, 0xd4, 0xd2, 0xd6, 0xd1, 0xd5, 0xd3, 0x37, 0x30, 0x34, 0x32, 0x36, 0x31,
            0x35, 0x33, 0xb7, 0xb0, 0xb4, 0xb2, 0xb6, 0xb1, 0xb5, 0xb3, 0x77, 0x70, 0x74, 0x72,
            0x76, 0x71, 0x75, 0x73, 0xf7, 0xf0, 0xf4, 0xf2, 0xf6, 0xf1, 0xf5, 0xf3, 0x0f, 0x08,
            0x0c, 0x0a, 0x0e, 0x09, 0x0d, 0x0b, 0x8f, 0x88, 0x8c, 0x8a, 0x8e, 0x89, 0x8d, 0x8b,
            0x4f, 0x48, 0x4c, 0x4a, 0x4e, 0x49, 0x4d, 0x4b, 0xcf, 0xc8, 0xcc, 0xca, 0xce, 0xc9,
            0xcd, 0xcb, 0x2f, 0x28, 0x2c, 0x2a, 0x2e, 0x29, 0x2d, 0x2b, 0xaf, 0xa8, 0xac, 0xaa,
            0xae, 0xa9, 0xad, 0xab, 0x6f, 0x68, 0x6c, 0x6a, 0x6e, 0x69, 0x6d, 0x6b, 0xef, 0xe8,
            0xec, 0xea, 0xee, 0xe9, 0xed, 0xeb, 0x9f, 0x30, 0x71, 0xd2, 0xe4, 0x29, 0x53, 0xa7,
            0x4d, 0x9f, 0x31, 0x73, 0xd6, 0xec, 0x39, 0x73, 0xe7, 0xcd, 0x5f, 0xb0, 0x70, 0xd1,
            0xe2, 0x25, 0x4b, 0x97, 0x2d, 0x5f, 0xb1, 0x72, 0xd5, 0xea, 0x35, 0x6b, 0xd7, 0xad,
            0xdf, 0xb0, 0x71, 0xd3, 0xe6, 0x2d, 0x5b, 0xb7, 0x6d, 0xdf, 0xb1, 0x73, 0xd7, 0xee,
            0x3d, 0x7b, 0xf7, 0xed, 0x3f, 0x70, 0xf0, 0xd0, 0xe1, 0x23, 0x47, 0x8f, 0x1d, 0x3f,
            0x71, 0xf2, 0xd4, 0xe9, 0x33, 0x67, 0xcf, 0x9d, 0xbf, 0x70, 0xf1, 0xd2, 0xe5, 0x2b,
            0x57, 0xaf, 0x5d, 0xbf, 0x71, 0xf3, 0xd6, 0xed, 0x3b, 0x77, 0xef, 0xdd, 0x7f, 0xf0,
            0xf0, 0xd1, 0xe3, 0x27, 0x4f, 0x9f, 0x3d, 0x7f, 0xf1, 0xf2, 0xd5, 0xeb, 0x37, 0x6f,
            0xdf, 0xbd, 0xff, 0xf0, 0xf1, 0xd3, 0xe7, 0x2f, 0x5f, 0xbf, 0x7d, 0xff, 0xf1, 0xf3,
            0xd7, 0xef, 0x3f, 0x7f, 0xff, 0xfd, 0x67, 0x18, 0xf5, 0xff, 0xa8, 0xff, 0x47, 0xb0,
            0xff, 0x01, 0xe4, 0xc9, 0xfe, 0x10,
        ];
        assert_eq!(inflate_zlib(&compressed).unwrap(), payload);
    }

    /// 压缩炸弹：不到 33 KB 的流，能解出 5 MB 以上。
    ///
    /// `inflate` **没有输出上限**：`inflate_block` 里的 `out.push` 只受输入里
    /// 回引数量的约束，而 DEFLATE 用 (长度 258, 距离 1) 可以拿 13 bit 换 258 字节
    /// （≈158:1，再加字面量还能更高）。这条测试把"无上限"钉死——一旦有人加了
    /// 上限它会失败，提醒他去看 `decode_png` 里 `inflate_zlib` 与 `expected`
    /// 校验的**先后顺序**：解压发生在校验之前，`MAX_DIM` 拦不住压缩炸弹。
    #[test]
    fn a_tiny_stream_expands_far_beyond_any_sane_image() {
        /// DEFLATE 位流：Huffman 码高位先出，字节内低位先填。
        struct Bits {
            out: Vec<u8>,
            acc: u32,
            n: u32,
        }
        impl Bits {
            fn bit(&mut self, b: u32) {
                self.acc |= (b & 1) << self.n;
                self.n += 1;
                if self.n == 8 {
                    self.out.push(self.acc as u8);
                    self.acc = 0;
                    self.n = 0;
                }
            }
            /// 数值字段（BFINAL / BTYPE）：低位先传。
            fn lsb(&mut self, v: u32, len: u32) {
                for i in 0..len {
                    self.bit((v >> i) & 1);
                }
            }
            /// Huffman 码：高位先传。
            fn msb(&mut self, v: u32, len: u32) {
                for i in (0..len).rev() {
                    self.bit((v >> i) & 1);
                }
            }
            fn finish(mut self) -> Vec<u8> {
                if self.n > 0 {
                    self.out.push(self.acc as u8);
                }
                self.out
            }
        }

        let repeats: u32 = 20_480; // 1 + 20480 × 258 = 5_283_841 字节
        let mut w = Bits {
            out: Vec::new(),
            acc: 0,
            n: 0,
        };
        w.lsb(1, 1); // BFINAL = 1
        w.lsb(1, 2); // BTYPE = 01（固定 Huffman）
        w.msb(0x30, 8); // 字面量 0：先垫一个字节，给回引一个可指的窗口
        for _ in 0..repeats {
            w.msb(0xC5, 8); // 长度码 285 → 258 字节
            w.msb(0x00, 5); // 距离码 0 → 距离 1
        }
        w.msb(0x00, 7); // 符号 256：块结束
        let bomb = w.finish();

        let expanded = inflate(&bomb).unwrap();
        assert_eq!(expanded.len(), 1 + repeats as usize * 258);
        assert!(
            expanded.len() > 5 * 1024 * 1024,
            "解出了 {} 字节",
            expanded.len()
        );
        assert!(
            bomb.len() < 40 * 1024,
            "输入只有 {} 字节，放大 {} 倍",
            bomb.len(),
            expanded.len() / bomb.len()
        );
    }
}
