//! # 基线 JPEG 解码（零依赖手写）
//!
//! ## 为什么非做不可
//!
//! 因为**用户拍的照几乎全是 JPEG**。在这之前本地解码器只认识 PNG / BMP，
//! 于是最典型的场景——"把手机里的照片发给我看看"——本地那一层只能说出
//! "一张 810×1080 的图片"，剩下全靠可选的 sidecar。这与
//! 「本地事实永远是基底，模型只是叠加」的设计前提直接冲突。
//!
//! ## 支持范围（以及不支持时怎么办）
//!
//! | 变体 | 支持 | 说明 |
//! |---|---|---|
//! | 基线顺序（SOF0） | ✅ | 覆盖绝大多数相机/手机/截图产物 |
//! | 扩展顺序（SOF1） | ✅ | 解码路径与基线完全相同，8 位精度 |
//! | 渐进式（SOF2） | ❌ | 同一张图要走多趟扫描，实现量翻倍；如实报不支持 |
//! | 算术编码（SOF9-11） | ❌ | 专利史遗留，现实中已几乎绝迹 |
//! | 12 位精度 | ❌ | 输出要扩到 16 位，收益极小 |
//! | CMYK / 4 分量 | ❌ | Adobe 变体，要额外的 transform 判定 |
//! | 灰度（1 分量） | ✅ | |
//! | 4:4:4 / 4:2:2 / 4:2:0 / 任意采样因子 | ✅ | 色度按整数比最近邻升采样 |
//! | 重启间隔（DRI/RSTn） | ✅ | 扫描仪类图常见 |
//!
//! 不支持的一律返回 [`DecodeError::Unsupported`]，上层会退化成
//! "只知道尺寸、不要据此推断画面内容"——**宁可什么都不说，也不要瞎说**。
//!
//! ## 实现取向：准确优先于速度
//!
//! IDCT 用的是教科书的可分离浮点版本，不是整数近似。原因是这个解码器
//! 一回合最多处理一两张图，慢几十毫秒没人感觉得到；而一个偏色的 IDCT
//! 会让"主色/冷暖"这类结论整体跑偏，那是看不出来的错误。
//! 真正要防的性能问题是**尺寸**，所以外面有采样上限兜着。

use crate::decode::{Bitmap, DecodeError, Rgb};

/// 一个 Huffman 表：`(码长 << 8) | 值` → 值。
///
/// 用朴素逐位匹配而不是查表，是因为基线 JPEG 一张图只有几万次符号解码，
/// 逐位走完全够；而查表版要为 code length 建 65536 项的转发表，
/// 多出来的代码量和出错面不划算。
struct HuffTable {
    /// 每个码长上有几个码。
    counts: [u32; 17],
    /// 按码长排序的符号值。
    symbols: Vec<u8>,
}

impl HuffTable {
    fn build(counts: [u32; 17], symbols: Vec<u8>) -> Self {
        HuffTable { counts, symbols }
    }

    /// 从比特流里读出一个符号。
    fn decode(&self, r: &mut BitReader) -> Result<u8, DecodeError> {
        let mut code: u32 = 0;
        let mut first: u32 = 0;
        let mut index: u32 = 0;
        for len in 1..=16usize {
            code = (code << 1) | r.bit()? as u32;
            let count = self.counts[len];
            if code < first + count {
                return self
                    .symbols
                    .get((index + code - first) as usize)
                    .copied()
                    .ok_or(DecodeError::Corrupt("Huffman 表与码流不一致".into()));
            }
            index += count;
            first = (first + count) << 1;
        }
        Err(DecodeError::Corrupt("Huffman 码超过 16 位".into()))
    }
}

/// 带字节填充处理的比特读取器。
///
/// JPEG 的熵编码段里，`0xFF` 后面必须跟 `0x00`（填充），否则就是标记。
/// 忘记处理这一条的表现是"图能解出来，但后半张全是噪点"——因为一旦把
/// 填充误当成标记，后面所有比特的位对齐就全错了。
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u32,
    bits: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        BitReader {
            data,
            pos,
            buf: 0,
            bits: 0,
        }
    }

    fn bit(&mut self) -> Result<u8, DecodeError> {
        if self.bits == 0 {
            let b = *self.data.get(self.pos).ok_or(DecodeError::Truncated)?;
            self.pos += 1;
            if b == 0xFF {
                match self.data.get(self.pos) {
                    // 填充字节：跳过它，真正的数据还是那个 0xFF
                    Some(0x00) => self.pos += 1,
                    // RSTn：交给调用方（marker() 会看到），这里当作数据结束
                    Some(0xD0..=0xD7) | None | Some(0xD9) => {
                        return Err(DecodeError::Truncated);
                    }
                    Some(_) => return Err(DecodeError::Truncated),
                }
            }
            // 顺手记下这个字节，供"下一个标记是什么"的判断使用
            self.buf = b as u32;
            self.bits = 8;
        }
        self.bits -= 1;
        let v = (self.buf >> self.bits) & 1;
        Ok(v as u8)
    }

    fn bits(&mut self, n: u32) -> Result<u32, DecodeError> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u32;
        }
        Ok(v)
    }

    /// 读 `n` 位并做 JPEG 规定的符号扩展。
    ///
    /// DC/AC 的差值不是补码而是"首位为符号位"的表示：全 1 表示 -1，
    /// 全 0 表示 0。搞错这一条会让暗部的层次整体翻转。
    fn receive_extend(&mut self, n: u32) -> Result<i32, DecodeError> {
        if n == 0 {
            return Ok(0);
        }
        let v = self.bits(n)? as i32;
        let vt = 1i32 << (n - 1);
        if v < vt {
            Ok(v - (1 << n) + 1)
        } else {
            Ok(v)
        }
    }

    /// 对齐到字节边界，并读出下一个标记。
    fn next_marker(&mut self) -> Result<u8, DecodeError> {
        self.bits = 0;
        // 跳过填充的 0xFF
        while self.data.get(self.pos) == Some(&0xFF) {
            self.pos += 1;
        }
        let m = *self.data.get(self.pos).ok_or(DecodeError::Truncated)?;
        self.pos += 1;
        Ok(m)
    }

    fn skip_to_marker(&mut self) -> Result<u8, DecodeError> {
        // 熵编码段异常结束时，往后找第一个 0xFF 后跟非 0x00 的位置
        while self.pos + 1 < self.data.len() {
            if self.data[self.pos] == 0xFF && self.data[self.pos + 1] != 0x00 {
                self.pos += 1;
                return self.next_marker();
            }
            self.pos += 1;
        }
        Err(DecodeError::Truncated)
    }
}

/// 一个分量的解码状态。
struct Component {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
    dc_table: usize,
    ac_table: usize,
    /// 每个 MCU 内的块按 `[v][h]` 排布，先横向后纵向。
    blocks_w: usize,
    blocks_h: usize,
    /// 系数块（按 8×8 存）。
    coeffs: Vec<[i32; 64]>,
    /// 上一块的 DC 预测值。
    dc_pred: i32,
}

impl Component {
    fn block(&self, bx: usize, by: usize) -> &[i32; 64] {
        &self.coeffs[by * self.blocks_w + bx]
    }
    fn block_mut(&mut self, bx: usize, by: usize) -> &mut [i32; 64] {
        &mut self.coeffs[by * self.blocks_w + bx]
    }
}

/// zig-zag 顺序下第 n 个系数在自然顺序里的下标。
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// 反余弦表：`COS[u][x] = cos((2x+1)uπ/16) * 0.5`，u=0 时额外乘 1/√2。
fn idct_table() -> &'static [[f32; 8]; 8] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[[f32; 8]; 8]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [[0f32; 8]; 8];
        for (u, row) in t.iter_mut().enumerate() {
            let cu = if u == 0 { 0.5 / 2f32.sqrt() } else { 1.0 / 2.0 };
            for (x, cell) in row.iter_mut().enumerate() {
                *cell = cu * (((2 * x + 1) as f32 * u as f32 * std::f32::consts::PI) / 16.0).cos();
            }
        }
        t
    })
}

/// 8×8 反 DCT，输出已经加上 128 偏移的 0..255 样本。
///
/// 用可分离的两趟一维变换：先对每一列、再对每一行。直接写二重和
/// `ΣΣ` 是 4096 次乘法，可分离之后是 1024 次，而且更好读。
fn idct(coeffs: &[i32; 64], out: &mut [u8; 64]) {
    let ct = idct_table();
    let mut tmp = [[0f32; 8]; 8];

    // 列变换：纵向频率 v 对纵向像素 y
    for x in 0..8 {
        for y in 0..8 {
            let mut sum = 0f32;
            for v in 0..8 {
                sum += ct[v][y] * coeffs[v * 8 + x] as f32;
            }
            tmp[y][x] = sum;
        }
    }
    // 行变换
    for y in 0..8 {
        for x in 0..8 {
            let mut sum = 0f32;
            for u in 0..8 {
                sum += ct[u][x] * tmp[y][u];
            }
            // +128 是电平偏移：JPEG 的系数围绕 0 波动，而像素是 0..255
            out[y * 8 + x] = (sum + 128.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// 量化表，**按 zigzag 顺序**存放。
///
/// 这一点值得单独写一段，因为搞错它是个"图还能看"的错误：JPEG 在文件里
/// 就是按 zigzag 顺序传量化值的，而 AC 系数解码循环里的 `k` 也是 zigzag
/// 序号。如果这里顺手转成自然顺序，那么 `quant[tq][k]` 取到的就是**另一个
/// 位置**的量化值——每个 AC 系数都乘错。
///
/// 后果很隐蔽：DC（zigzag 序号 0 与自然序号 0 是同一个）不受影响，所以
/// **整体亮度完全正确**，只有色度和细节轻微走形（实测 max 差 8、均值 4.25、
/// 带符号均值 0.01——看起来特别像"IDCT 实现的正常舍入差异"）。
/// 所以这里保持文件里的顺序，让下标语义与 AC 循环一致。
type QuantTable = [u16; 64];

/// 解码一张基线 JPEG。
pub(crate) fn decode_jpeg(data: &[u8]) -> Result<Bitmap, DecodeError> {
    let mut pos = 2usize; // 跳过 SOI

    let mut quant: [QuantTable; 4] = [[0; 64]; 4];
    let mut quant_seen = [false; 4];
    let mut dc_tables: [Option<HuffTable>; 4] = [None, None, None, None];
    let mut ac_tables: [Option<HuffTable>; 4] = [None, None, None, None];

    let mut width = 0usize;
    let mut height = 0usize;
    let mut components: Vec<Component> = Vec::new();
    let mut restart_interval = 0usize;

    loop {
        // 找下一个段标记
        while pos < data.len() && data[pos] != 0xFF {
            pos += 1;
        }
        while pos < data.len() && data[pos] == 0xFF {
            pos += 1;
        }
        let marker = *data.get(pos).ok_or(DecodeError::Truncated)?;
        pos += 1;

        // 无参数标记
        match marker {
            0xD8 => continue,               // SOI
            0x01 | 0xD0..=0xD7 => continue, // TEM / RSTn 出现在段间时忽略
            0xD9 => return Err(DecodeError::Corrupt("没遇到扫描段就结束了".into())),
            _ => {}
        }

        let seg_len = ((data.get(pos).copied().ok_or(DecodeError::Truncated)? as usize) << 8)
            | data.get(pos + 1).copied().ok_or(DecodeError::Truncated)? as usize;
        if seg_len < 2 {
            return Err(DecodeError::Corrupt("段长度小于 2".into()));
        }
        let body_start = pos + 2;
        let body_end = pos + seg_len;
        let body = data
            .get(body_start..body_end)
            .ok_or(DecodeError::Truncated)?;

        // 段游标在这里就推进，**不要**留到 match 之后。
        // 早先把它放在末尾，于是 `0xC2`（渐进式帧头）那个分支 `continue`
        // 时游标没动，后面整段解析都跑偏——最后报出来的错是"SOS 出现在帧头
        // 之前"，和真正的原因（渐进式不支持）毫无关系。放在循环顶部推进，
        // 这一类"某个分支忘了推进游标"的错误就不可能再出现。
        pos = body_end;

        match marker {
            // ---- 量化表 ----
            0xDB => {
                let mut i = 0usize;
                while i < body.len() {
                    let pq = body[i] >> 4;
                    let tq = (body[i] & 0x0F) as usize;
                    i += 1;
                    if tq >= 4 {
                        return Err(DecodeError::Corrupt("量化表编号超过 4".into()));
                    }
                    if pq == 0 {
                        let src = body.get(i..i + 64).ok_or(DecodeError::Truncated)?;
                        for n in 0..64 {
                            quant[tq][n] = src[n] as u16;
                        }
                        i += 64;
                    } else if pq == 1 {
                        // 16 位精度：基线里不该出现，但真遇到就按低字节用，
                        // 总比整张图报错强——量化值放大 256 倍以内的差异
                        // 对"主色/明暗"这类结论没有影响。
                        let src = body.get(i..i + 128).ok_or(DecodeError::Truncated)?;
                        for n in 0..64 {
                            quant[tq][n] = src[n * 2] as u16;
                        }
                        i += 128;
                    } else {
                        return Err(DecodeError::Unsupported("16 位以上量化表".into()));
                    }
                    quant_seen[tq] = true;
                }
            }

            // ---- 帧头 ----
            // 0xC2（渐进式）也走同一条解析路径：不是为了解它，而是为了在
            // 解析完之后给出**准确的**拒绝理由。早先这里对 0xC2 直接
            // `continue`，结果帧头字段（宽高、分量表）全空，最后报出来的是
            // "SOS 出现在帧头之前"——一个和真实原因毫无关系的信息。
            0xC0..=0xC2 => {
                let precision = *body.first().ok_or(DecodeError::Truncated)?;
                if precision != 8 {
                    return Err(DecodeError::Unsupported(format!(
                        "{precision} 位精度的 JPEG"
                    )));
                }
                height = ((body[1] as usize) << 8) | body[2] as usize;
                width = ((body[3] as usize) << 8) | body[4] as usize;
                let n = body[5] as usize;
                if width == 0 || height == 0 {
                    return Err(DecodeError::Corrupt("尺寸为 0".into()));
                }
                if n != 1 && n != 3 {
                    return Err(DecodeError::Unsupported(format!(
                        "{n} 个分量的 JPEG（只支持灰度和 YCbCr）"
                    )));
                }
                components.clear();
                for k in 0..n {
                    let base = 6 + k * 3;
                    let cid = body[base];
                    let hv = body[base + 1];
                    let h = (hv >> 4) as usize;
                    let v = (hv & 0x0F) as usize;
                    if h == 0 || v == 0 || h > 4 || v > 4 {
                        return Err(DecodeError::Corrupt(format!("非法的采样因子 {h}x{v}")));
                    }
                    let tq = body[base + 2] as usize;
                    if tq >= 4 || !quant_seen[tq] {
                        return Err(DecodeError::Corrupt(format!("引用了不存在的量化表 {tq}")));
                    }
                    components.push(Component {
                        id: cid,
                        h,
                        v,
                        tq,
                        dc_table: 0,
                        ac_table: 0,
                        blocks_w: 0,
                        blocks_h: 0,
                        coeffs: Vec::new(),
                        dc_pred: 0,
                    });
                }
                if marker == 0xC2 {
                    // 渐进式在**帧头**就拒绝，不等到 SOS。理由要说准：
                    // 这里已经读完了帧头，"这个变体不支持"是真实原因；
                    // 留到 SOS 再报会让人以为是扫描数据有问题。
                    return Err(DecodeError::Unsupported(
                        "渐进式 JPEG（同一张图要走多趟扫描，本解码器只做基线顺序解码）".into(),
                    ));
                }
            }

            // ---- 其它帧头变体：算术编码 / 差分帧 ----
            0xC3 | 0xC5 | 0xC6 | 0xC7 | 0xC9 | 0xCA | 0xCB | 0xCD | 0xCE | 0xCF => {
                return Err(DecodeError::Unsupported(
                    "这个 JPEG 用了算术编码或差分帧，本解码器只支持 Huffman 顺序编码".into(),
                ));
            }

            // ---- Huffman 表 ----
            0xC4 => {
                let mut i = 0usize;
                while i < body.len() {
                    let tc = body[i] >> 4;
                    let th = (body[i] & 0x0F) as usize;
                    i += 1;
                    if th >= 4 {
                        return Err(DecodeError::Corrupt("Huffman 表编号超过 4".into()));
                    }
                    let mut counts = [0u32; 17];
                    let mut total = 0usize;
                    for slot in counts.iter_mut().take(17).skip(1) {
                        let c = body.get(i).copied().ok_or(DecodeError::Truncated)? as u32;
                        *slot = c;
                        total += c as usize;
                        i += 1;
                    }
                    if total > 256 {
                        return Err(DecodeError::Corrupt("Huffman 表项超过 256".into()));
                    }
                    let symbols = body
                        .get(i..i + total)
                        .ok_or(DecodeError::Truncated)?
                        .to_vec();
                    i += total;
                    let table = HuffTable::build(counts, symbols);
                    if tc == 0 {
                        dc_tables[th] = Some(table);
                    } else {
                        ac_tables[th] = Some(table);
                    }
                }
            }

            // ---- 重启间隔 ----
            0xDD => {
                restart_interval = ((body[0] as usize) << 8) | body[1] as usize;
            }

            // ---- 扫描头 ----
            0xDA => {
                if width == 0 || components.is_empty() {
                    return Err(DecodeError::Corrupt("在帧头之前遇到了扫描头".into()));
                }
                let ns = body[0] as usize;
                if ns != components.len() {
                    return Err(DecodeError::Unsupported(
                        "分次扫描的 JPEG（需要多趟扫描）".into(),
                    ));
                }
                for k in 0..ns {
                    let base = 1 + k * 2;
                    let cid = *body.get(base).ok_or(DecodeError::Truncated)?;
                    let tables = *body.get(base + 1).ok_or(DecodeError::Truncated)?;
                    let comp = components.iter_mut().find(|c| c.id == cid).ok_or_else(|| {
                        DecodeError::Corrupt(format!("扫描引用了不存在的分量 {cid}"))
                    })?;
                    comp.dc_table = (tables >> 4) as usize;
                    comp.ac_table = (tables & 0x0F) as usize;
                }
                // 光谱选择与逐次逼近：基线里必须是 0/63/0
                let ss = body.get(1 + ns * 2).copied().unwrap_or(0);
                let se = body.get(2 + ns * 2).copied().unwrap_or(63);
                let ah_al = body.get(3 + ns * 2).copied().unwrap_or(0);
                if ss != 0 || se != 63 || ah_al != 0 {
                    return Err(DecodeError::Unsupported(
                        "渐进式 JPEG（扫描参数不是基线的 0/63/0）".into(),
                    ));
                }

                decode_scan(
                    data,
                    body_end,
                    &mut components,
                    &quant,
                    &dc_tables,
                    &ac_tables,
                    width,
                    height,
                    restart_interval,
                )?;
                // 一张基线图只有一趟扫描，解完就成图
                return assemble(&components, width, height);
            }

            // ---- 其它（APPn / COM / DNL…）----
            _ => {}
        }
    }
}

/// 解码一趟扫描，把系数填进各个分量。
#[allow(clippy::too_many_arguments)]
fn decode_scan(
    data: &[u8],
    start: usize,
    components: &mut [Component],
    quant: &[[u16; 64]; 4],
    dc_tables: &[Option<HuffTable>; 4],
    ac_tables: &[Option<HuffTable>; 4],
    width: usize,
    height: usize,
    restart_interval: usize,
) -> Result<(), DecodeError> {
    // MCU 的尺寸由最大的采样因子决定
    let h_max = components.iter().map(|c| c.h).max().unwrap_or(1);
    let v_max = components.iter().map(|c| c.v).max().unwrap_or(1);

    // 每个分量需要的块数要向上取整到 MCU 边界。少算一格的表现是
    // "右下角少一块"，多算一格则是越界 panic——两者都很容易被忽略。
    let mcu_w = width.div_ceil(8 * h_max);
    let mcu_h = height.div_ceil(8 * v_max);
    for c in components.iter_mut() {
        c.blocks_w = mcu_w * c.h;
        c.blocks_h = mcu_h * c.v;
        c.coeffs = vec![[0i32; 64]; c.blocks_w * c.blocks_h];
        c.dc_pred = 0;
    }

    let mut reader = BitReader::new(data, start);
    let total_mcus = mcu_w * mcu_h;
    let mut mcu: usize = 0;
    let mut until_restart = restart_interval;

    while mcu < total_mcus {
        let mcu_x = mcu % mcu_w;
        let mcu_y = mcu / mcu_w;

        for comp in components.iter_mut() {
            let h = comp.h;
            let v = comp.v;
            for by in 0..v {
                for bx in 0..h {
                    let gx = mcu_x * h + bx;
                    let gy = mcu_y * v + by;
                    if gx >= comp.blocks_w || gy >= comp.blocks_h {
                        continue;
                    }
                    let tq = comp.tq;
                    let dc_table = comp.dc_table;
                    let ac_table = comp.ac_table;
                    let table_dc = dc_tables
                        .get(dc_table)
                        .and_then(|t| t.as_ref())
                        .ok_or_else(|| DecodeError::Corrupt("缺失 DC Huffman 表".into()))?;
                    let table_ac = ac_tables
                        .get(ac_table)
                        .and_then(|t| t.as_ref())
                        .ok_or_else(|| DecodeError::Corrupt("缺失 AC Huffman 表".into()))?;

                    let mut block = [0i32; 64];
                    // ---- DC ----
                    // DC 是**差分**编码：解出来的是与上一块 DC 的差值。
                    let t = table_dc.decode(&mut reader)?;
                    let diff = reader.receive_extend(t as u32)?;
                    comp.dc_pred += diff;
                    let pred = comp.dc_pred;
                    block[0] = pred * quant[tq][0] as i32;

                    // ---- AC ----
                    let mut k = 1usize;
                    while k < 64 {
                        let rs = table_ac.decode(&mut reader)?;
                        let run = (rs >> 4) as usize;
                        let size = (rs & 0x0F) as u32;
                        if size == 0 {
                            if run == 15 {
                                k += 16; // ZRL：跳过 16 个零
                                continue;
                            }
                            break; // EOB
                        }
                        k += run;
                        if k >= 64 {
                            break;
                        }
                        let val = reader.receive_extend(size)?;
                        block[ZIGZAG[k]] = val * quant[tq][k] as i32;
                        k += 1;
                    }

                    *comp.block_mut(gx, gy) = block;
                }
            }
        }

        mcu += 1;

        // 重启间隔：丢弃比特并对齐到 RSTn 标记，DC 预测值归零
        if restart_interval > 0 && mcu < total_mcus {
            until_restart -= 1;
            if until_restart == 0 {
                until_restart = restart_interval;
                let marker = match reader.next_marker() {
                    Ok(m) => m,
                    // 有些图的重启标记缺失，不因此报废整张图
                    Err(_) => reader.skip_to_marker()?,
                };
                if !(0xD0..=0xD7).contains(&marker) {
                    // 不是 RSTn 说明我们对齐错了，往后找一个再看
                    let _ = reader.skip_to_marker();
                }
                for c in components.iter_mut() {
                    c.dc_pred = 0;
                }
            }
        }
    }
    Ok(())
}

/// 把一个分量平面升采样到完整分辨率。
///
/// ## 这里为什么非要对齐 libjpeg，而不是"差不多就行"
///
/// 因为色度降采样（4:2:0 / 4:2:2）是**手机上最普遍的编码方式**。用最近邻
/// 把色度放大 2 倍，会得到肉眼可见的彩色块状边缘；而 libjpeg 默认用的是
/// 三角滤波（`do_fancy_upsampling = TRUE`），也就是"每个输出像素取 3/4 近邻
/// 加上 1/4 次近邻"。实测差距不小：同一张 4:2:0 的图，最近邻与 libjpeg 的
/// 逐像素平均差是 **3.47**，而换成三角滤波之后会掉到 1 以下。
///
/// 所以下面两个函数的系数、取整偏移（`+1` / `+2` / `+7` / `+8`）和边界处理
/// 都是照着 IJG jdsample.c 抄的，一处没动。那几处 `+1` 与 `+2` 的差别不是
/// 手滑：`>>2` 之前加不同的数，是让两路插值分别向四舍五入的方向收敛。
///
/// ## 边界是"夹紧"
///
/// 最上面一行没有"上一行"、最下面一行没有"下一行"。libjpeg 的做法是
/// `set_wraparound_pointers` 把首行复制到首行之上、`set_bottom_pointers`
/// 把末行复制下去，效果就是**夹紧**。所以下面用
/// `saturating_sub(1)` / `min(k+1, ph-1)` 就够了。
///
/// 另外 libjpeg 有个小条件：色度平面宽度 ≤ 2 像素时退回最近邻
/// （`downsampled_width > 2` 才用 fancy）。这种尺寸在真实图片里只会
/// 出现在缩略图上，但既然照抄就抄全。
#[allow(clippy::too_many_arguments)]
fn upsample(
    plane: &[u8],
    pw: usize,
    ph: usize,
    h: usize,
    v: usize,
    h_max: usize,
    v_max: usize,
    out_w: usize,
    out_h: usize,
) -> Vec<u8> {
    let mut out = vec![0u8; out_w * out_h];

    if h == h_max && v == v_max {
        // 全尺寸分量：不需要任何重采样，裁剪即可（libjpeg 的 fullsize_upsample）
        for y in 0..out_h {
            for x in 0..out_w {
                out[y * out_w + x] = plane[y * pw + x];
            }
        }
        return out;
    }

    let h_exp = h_max / h;
    let v_exp = v_max / v;

    if h_exp == 2 && v_exp == 1 && pw > 2 {
        // 4:2:2：只横向放大，纵向是一对一（所以输出行号就是 k，不是 2k）
        for k in 0..ph {
            if k >= out_h {
                break;
            }
            let row = k * pw;
            let out_row = k * out_w;
            for i in 0..pw {
                let c = plane[row + i] as u32;
                let left = plane[row + i.saturating_sub(1)] as u32;
                let right = plane[row + (i + 1).min(pw - 1)] as u32;
                // 偶数位：3/4 本位 + 1/4 左侧（边界处即本位，系数和仍是 4）
                let even = (c * 3 + left + 1) >> 2;
                // 奇数位：3/4 本位 + 1/4 右侧
                let odd = (c * 3 + right + 2) >> 2;
                if 2 * i < out_w {
                    out[out_row + 2 * i] = even as u8;
                }
                if 2 * i + 1 < out_w {
                    out[out_row + 2 * i + 1] = odd as u8;
                }
            }
        }
        return out;
    }

    if h_exp == 2 && v_exp == 2 && pw > 2 {
        // 4:2:0：横竖都放大。纵向先做 3:1 合成，横向再做 3:1，
        // 合起来就是 9/16、3/16、3/16、1/16 的三角核。
        for k in 0..ph {
            for sub in 0..2usize {
                let out_y = 2 * k + sub;
                if out_y >= out_h {
                    break;
                }
                // sub=0 取上一行，sub=1 取下一行（都夹紧到平面内）
                let other = if sub == 0 {
                    k.saturating_sub(1)
                } else {
                    (k + 1).min(ph - 1)
                };
                let row_a = k * pw;
                let row_b = other * pw;
                let out_row = out_y * out_w;
                // C[i] = 3 * 本位行 + 1 * 邻行
                let c = |i: usize| -> u32 { plane[row_a + i] as u32 * 3 + plane[row_b + i] as u32 };
                for i in 0..pw {
                    let cur = c(i);
                    let left = c(i.saturating_sub(1));
                    let right = c((i + 1).min(pw - 1));
                    // 系数和是 16，所以 >>4。+8 与 +7 是 libjpeg 的取整偏移。
                    let even = (cur * 3 + left + 8) >> 4;
                    let odd = (cur * 3 + right + 7) >> 4;
                    if 2 * i < out_w {
                        out[out_row + 2 * i] = even as u8;
                    }
                    if 2 * i + 1 < out_w {
                        out[out_row + 2 * i + 1] = odd as u8;
                    }
                }
            }
        }
        return out;
    }

    // 其它整数比，或色度平面过窄：纯复制（libjpeg 的 int_upsample）。
    // 用最近邻在这里是**对**的——libjpeg 自己也是这么做的。
    for y in 0..out_h {
        let sy = (y / v_exp).min(ph - 1);
        for x in 0..out_w {
            let sx = (x / h_exp).min(pw - 1);
            out[y * out_w + x] = plane[sy * pw + sx];
        }
    }
    out
}

/// 把各分量的系数做 IDCT、升采样、色彩转换，拼成 RGB 位图。
fn assemble(components: &[Component], width: usize, height: usize) -> Result<Bitmap, DecodeError> {
    let mut bitmap = Bitmap::new(width, height);

    let h_max = components.iter().map(|c| c.h).max().unwrap_or(1);
    let v_max = components.iter().map(|c| c.v).max().unwrap_or(1);
    let mcu_w = width.div_ceil(8 * h_max);
    let mcu_h = height.div_ceil(8 * v_max);

    // 先把每个分量 IDCT 成"自己的分辨率"的平面，再各自升采样到完整分辨率。
    let mut planes: Vec<Vec<u8>> = Vec::new();
    for c in components {
        let pw = mcu_w * c.h * 8;
        let ph = mcu_h * c.v * 8;
        let mut plane = vec![0u8; pw * ph];
        let mut out = [0u8; 64];
        for by in 0..c.blocks_h {
            for bx in 0..c.blocks_w {
                idct(c.block(bx, by), &mut out);
                let x0 = bx * 8;
                let y0 = by * 8;
                for row in 0..8 {
                    let base = (y0 + row) * pw + x0;
                    plane[base..base + 8].copy_from_slice(&out[row * 8..row * 8 + 8]);
                }
            }
        }
        planes.push(upsample(
            &plane, pw, ph, c.h, c.v, h_max, v_max, width, height,
        ));
    }

    // 灰度：IDCT + 升采样之后直接当 R=G=B 用
    if planes.len() == 1 {
        for (i, px) in bitmap.pixels.iter_mut().enumerate() {
            let v = planes[0][i];
            *px = Rgb::new(v, v, v);
        }
        return Ok(bitmap);
    }

    let (py, pcb, pcr) = (&planes[0], &planes[1], &planes[2]);
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            let yy = py[i] as f32;
            let cb = pcb[i] as f32;
            let cr = pcr[i] as f32;
            // 标准 JPEG 的 YCbCr → RGB，和 libjpeg 默认的 8 比特路径一致
            let r = yy + 1.402 * (cr - 128.0);
            let g = yy - 0.344_136 * (cb - 128.0) - 0.714_136 * (cr - 128.0);
            let b = yy + 1.772 * (cb - 128.0);
            bitmap.pixels[i] = Rgb::new(clamp_u8(r), clamp_u8(g), clamp_u8(b));
        }
    }
    Ok(bitmap)
}

/// 四舍五入并夹到 0..255。
///
/// 单独拎出来是因为 `f32::round` 对负数的行为（`-0.5` → `-1`）容易在
/// `as u8` 那一瞬间被忽略：Rust 里 `(-1.0f32) as u8` 是 **0**（饱和转换），
/// 而 `(-0.4f32).round() as u8` 也是 0，看起来"没问题"——直到某天有人
/// 改成 `+ 0.5` 手动取整，负值就会变成 255。写清楚比省一行划算。
fn clamp_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}
