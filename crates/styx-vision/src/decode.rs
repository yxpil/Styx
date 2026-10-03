//! # 图像解码
//!
//! 三种格式，三种待遇，都是刻意的：
//!
//! | 格式 | 能力 | 理由 |
//! |---|---|---|
//! | **PNG** | 完整像素 | 无损 + zlib，手写 inflate 就能拿到真像素 |
//! | **BMP** | 完整像素 | 无压缩直存，几乎是白送的 |
//! | **JPEG** | 仅尺寸 / 方向 | 完整的 baseline 解码器要上千行（Huffman + 反量化 + IDCT + 上采样），维护成本远高于收益 |
//!
//! JPEG 那条缺口不是死路：前端会用浏览器的 Canvas 对**任意格式**做同一套
//! 像素统计并随上传一起提交（见 `web/app.js` 的 `analyzeInCanvas`）。
//! 于是实际链路是——浏览器能解的，浏览器解；解不了的（CLI 直接丢进来的 PNG/BMP），
//! 本地解。两条路都拿不到时，`ImageFacts::decoded` 会是 `false`，
//! 上层据此给出诚实的降级说明，而不是假装看见了。

use crate::inflate::inflate_zlib;

/// 一个 8 位 RGB 像素。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    /// `#rrggbb`。
    pub fn hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// 感知亮度 0..1（Rec.601）。
    pub fn luma(&self) -> f32 {
        (0.299 * self.r as f32 + 0.587 * self.g as f32 + 0.114 * self.b as f32) / 255.0
    }

    /// 饱和度 0..1（max-min 归一）。
    pub fn saturation(&self) -> f32 {
        let mx = self.r.max(self.g).max(self.b) as f32;
        let mn = self.r.min(self.g).min(self.b) as f32;
        (mx - mn) / 255.0
    }

    /// HSL 里的 L：`(max+min)/2`。
    ///
    /// 与 [`Rgb::luma`] 的区别在这里很关键：纯红的 luma 只有 0.30（绿色的
    /// 权重高达 0.587），照 luma 判深浅会把正红叫成"暗红"。而人眼说的
    /// "深红/浅红"看的是 HSL 的 L，纯红正好是 0.5。所以**给颜色起名**用
    /// 这个，**判断画面明暗**才用 luma。
    pub fn lightness(&self) -> f32 {
        let mx = self.r.max(self.g).max(self.b) as f32;
        let mn = self.r.min(self.g).min(self.b) as f32;
        (mx + mn) / 2.0 / 255.0
    }

    /// 色相角度（0..360），灰色时返回 0。
    pub fn hue(&self) -> f32 {
        let r = self.r as f32 / 255.0;
        let g = self.g as f32 / 255.0;
        let b = self.b as f32 / 255.0;
        let mx = r.max(g).max(b);
        let mn = r.min(g).min(b);
        let d = mx - mn;
        if d <= f32::EPSILON {
            return 0.0;
        }
        let h = if mx == r {
            ((g - b) / d) % 6.0
        } else if mx == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        let h = h * 60.0;
        if h < 0.0 {
            h + 360.0
        } else {
            h
        }
    }
}

/// 解出来的位图（一律转成 RGB8，丢掉 alpha）。
///
/// 丢掉 alpha 是有意的：角色扮演关心的是"这张图看起来什么样"，
/// 而不是"图层的透明度"。PNG 的透明区域在视觉上通常被当作白底或黑底，
/// 我们统一按"与白底合成"处理，这最接近人眼看预览图的经验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<Rgb>,
}

impl Bitmap {
    /// 尺寸已定、内容为黑的位图。crate 内共用（PNG / BMP / JPEG 都要）。
    pub(crate) fn new(width: usize, height: usize) -> Self {
        Bitmap {
            width,
            height,
            pixels: vec![Rgb::default(); width * height],
        }
    }

    /// 越界写入静默丢弃：解码器在边界块的填充区会算到图像外的坐标，
    /// 让它每次调用都判边界只会把代码弄脏，而"丢弃"正是想要的行为。
    pub(crate) fn set(&mut self, x: usize, y: usize, c: Rgb) {
        if x < self.width && y < self.height {
            self.pixels[y * self.width + x] = c;
        }
    }

    pub fn get(&self, x: usize, y: usize) -> Rgb {
        self.pixels
            .get(y * self.width + x)
            .copied()
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0 || self.pixels.is_empty()
    }
}

/// 解码失败的原因（不是崩溃，是"看不懂"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// 数据太短/被截断。
    Truncated,
    /// 魔数不匹配。
    NotAnImage,
    /// 认得出格式，但这个变体不支持（如 16 位 PNG、隔行扫描、CMYK JPEG）。
    Unsupported(String),
    /// 数据内部不一致（CRC 对不上、结构错乱）。
    Corrupt(String),
    /// 解压失败。
    Inflate(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "文件像是被截断了"),
            DecodeError::NotAnImage => write!(f, "认不出这是什么图片格式"),
            DecodeError::Unsupported(what) => write!(f, "暂不支持这种变体：{what}"),
            DecodeError::Corrupt(what) => write!(f, "数据不一致：{what}"),
            DecodeError::Inflate(what) => write!(f, "解压失败：{what}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// 认出来的格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Bmp,
    Jpeg,
}

impl Format {
    pub fn as_str(&self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Bmp => "bmp",
            Format::Jpeg => "jpeg",
        }
    }
}

/// 靠魔数认格式（不信任扩展名）。
pub fn sniff(data: &[u8]) -> Option<Format> {
    if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some(Format::Png);
    }
    if data.starts_with(b"BM") && data.len() >= 14 {
        return Some(Format::Bmp);
    }
    if data.starts_with(&[0xff, 0xd8]) {
        return Some(Format::Jpeg);
    }
    None
}

/// 解出像素。JPEG 会返回 [`DecodeError::Unsupported`]。
pub fn decode(data: &[u8]) -> Result<Bitmap, DecodeError> {
    match sniff(data) {
        Some(Format::Png) => decode_png(data),
        Some(Format::Bmp) => decode_bmp(data),
        Some(Format::Jpeg) => crate::jpeg::decode_jpeg(data),
        None => Err(DecodeError::NotAnImage),
    }
}

/// 只读尺寸（三种格式都支持）。
pub fn dimensions(data: &[u8]) -> Option<(usize, usize)> {
    match sniff(data)? {
        Format::Png => png_dimensions(data),
        Format::Bmp => bmp_dimensions(data),
        Format::Jpeg => jpeg_dimensions(data),
    }
}

// --------------------------------------------------------------------- PNG

const PNG_SIG: usize = 8;

fn png_dimensions(data: &[u8]) -> Option<(usize, usize)> {
    // 布局：8 字节签名 ｜ 4 字节长度 ｜ 4 字节块类型（"IHDR"）｜ 宽 ｜ 高
    // 所以宽在 8+4+4=16，高在 20。少算一个块类型就会读到 "IHDR" 的 ASCII 值。
    if data.get(PNG_SIG + 4..PNG_SIG + 8)? != b"IHDR" {
        return None;
    }
    let w = be32(data.get(PNG_SIG + 8..PNG_SIG + 12)?)?;
    let h = be32(data.get(PNG_SIG + 12..PNG_SIG + 16)?)?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w as usize, h as usize))
}

fn be32(b: &[u8]) -> Option<u32> {
    if b.len() < 4 {
        return None;
    }
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn be16(b: &[u8]) -> Option<u16> {
    if b.len() < 2 {
        return None;
    }
    Some(u16::from_be_bytes([b[0], b[1]]))
}

/// 单张图的行数上限，防止一个小文件声称自己是 4 万像素高的图。
const MAX_DIM: usize = 20000;

pub(crate) fn decode_png(data: &[u8]) -> Result<Bitmap, DecodeError> {
    if data.len() < PNG_SIG + 25 {
        return Err(DecodeError::Truncated);
    }

    let width = be32(&data[16..20]).ok_or(DecodeError::Truncated)? as usize;
    let height = be32(&data[20..24]).ok_or(DecodeError::Truncated)? as usize;
    let bit_depth = data[24];
    let color_type = data[25];
    let compression = data[26];
    let filter_method = data[27];
    let interlace = data[28];

    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return Err(DecodeError::Corrupt(format!(
            "尺寸不合理：{width}×{height}"
        )));
    }
    if compression != 0 || filter_method != 0 {
        return Err(DecodeError::Unsupported("非标准的压缩/过滤方法".into()));
    }
    if interlace != 0 {
        return Err(DecodeError::Unsupported("隔行扫描（Adam7）的 PNG".into()));
    }
    if bit_depth == 16 {
        return Err(DecodeError::Unsupported("16 位每通道的 PNG".into()));
    }
    if !matches!(bit_depth, 1 | 2 | 4 | 8) {
        return Err(DecodeError::Unsupported(format!("{bit_depth} 位深度")));
    }

    let channels = match color_type {
        0 => 1, // grayscale
        2 => 3, // RGB
        3 => 1, // palette（每像素一个索引）
        4 => 2, // gray + alpha
        6 => 4, // RGBA
        other => return Err(DecodeError::Unsupported(format!("颜色类型 {other}"))),
    };
    if color_type != 3 && bit_depth != 8 {
        // 灰度图允许低位深，RGB/RGBA 的低位深没有实际使用场景
        return Err(DecodeError::Unsupported(format!(
            "低位深的颜色类型 {color_type}"
        )));
    }

    // 收集 IDAT 与调色板
    let mut idat: Vec<u8> = Vec::new();
    let mut palette: Vec<Rgb> = Vec::new();
    let mut alpha: Vec<u8> = Vec::new();

    let mut pos = PNG_SIG;
    let mut saw_iend = false;
    while pos + 8 <= data.len() {
        let len = be32(&data[pos..pos + 4]).ok_or(DecodeError::Truncated)? as usize;
        let kind = data.get(pos + 4..pos + 8).ok_or(DecodeError::Truncated)?;
        let body_start = pos + 8;
        let body_end = body_start.checked_add(len).ok_or(DecodeError::Truncated)?;
        let body = data
            .get(body_start..body_end)
            .ok_or(DecodeError::Truncated)?;
        // CRC 也占 4 字节；末尾不足时按截断处理
        if body_end + 4 > data.len() {
            return Err(DecodeError::Truncated);
        }

        match kind {
            b"PLTE" => {
                for chunk in body.chunks_exact(3).take(256) {
                    palette.push(Rgb::new(chunk[0], chunk[1], chunk[2]));
                }
            }
            b"tRNS" => {
                alpha.extend_from_slice(body);
            }
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => {
                saw_iend = true;
                break;
            }
            _ => {}
        }
        pos = body_end + 4;
    }
    if idat.is_empty() {
        return Err(DecodeError::Corrupt("没有 IDAT 数据".into()));
    }
    let _ = saw_iend;

    if color_type == 3 && palette.is_empty() {
        return Err(DecodeError::Corrupt("索引色图却没有调色板".into()));
    }

    let raw = inflate_zlib(&idat).map_err(|e| DecodeError::Inflate(e.to_string()))?;

    let bits_per_pixel = channels * bit_depth as usize;
    let bpp = bits_per_pixel.div_ceil(8).max(1);
    let row_bytes = (width * bits_per_pixel).div_ceil(8);
    let expected = height
        .checked_mul(row_bytes + 1)
        .ok_or(DecodeError::Corrupt("尺寸溢出".into()))?;
    if raw.len() < expected {
        return Err(DecodeError::Corrupt(format!(
            "解压后只有 {} 字节，按头部该有 {expected}",
            raw.len()
        )));
    }

    // 反过滤
    let mut prev = vec![0u8; row_bytes];
    let mut cur = vec![0u8; row_bytes];
    let mut bmp = Bitmap::new(width, height);
    let mut offset = 0usize;

    for y in 0..height {
        let ft = raw[offset];
        offset += 1;
        cur.copy_from_slice(&raw[offset..offset + row_bytes]);
        offset += row_bytes;
        unfilter(ft, bpp, &mut cur, &prev)?;

        for x in 0..width {
            let px = read_pixel(&cur, x, bit_depth, color_type, channels, &palette, &alpha);
            bmp.set(x, y, px);
        }
        std::mem::swap(&mut prev, &mut cur);
    }

    Ok(bmp)
}

/// 逐字节反过滤（PNG 规范里的 5 种）。
fn unfilter(ft: u8, bpp: usize, cur: &mut [u8], prev: &[u8]) -> Result<(), DecodeError> {
    match ft {
        0 => {}
        1 => {
            for i in bpp..cur.len() {
                cur[i] = cur[i].wrapping_add(cur[i - bpp]);
            }
        }
        2 => {
            for i in 0..cur.len() {
                cur[i] = cur[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..cur.len() {
                let a = if i >= bpp { cur[i - bpp] as u16 } else { 0 };
                let b = prev[i] as u16;
                cur[i] = cur[i].wrapping_add(((a + b) / 2) as u8);
            }
        }
        4 => {
            for i in 0..cur.len() {
                let a = if i >= bpp { cur[i - bpp] as u16 } else { 0 };
                let b = prev[i] as u16;
                let c = if i >= bpp { prev[i - bpp] as u16 } else { 0 };
                cur[i] = cur[i].wrapping_add(paeth(a, b, c));
            }
        }
        other => return Err(DecodeError::Corrupt(format!("未知的过滤类型 {other}"))),
    }
    Ok(())
}

fn paeth(a: u16, b: u16, c: u16) -> u8 {
    let p = a as i32 + b as i32 - c as i32;
    let pa = (p - a as i32).abs();
    let pb = (p - b as i32).abs();
    let pc = (p - c as i32).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

fn read_pixel(
    row: &[u8],
    x: usize,
    bit_depth: u8,
    color_type: u8,
    channels: usize,
    palette: &[Rgb],
    alpha: &[u8],
) -> Rgb {
    match color_type {
        3 => {
            let idx = sample_index(row, x, bit_depth);
            let color = palette.get(idx).copied().unwrap_or(Rgb::new(0, 0, 0));
            let a = alpha.get(idx).copied().unwrap_or(255);
            over_white(color, a)
        }
        0 => {
            let v = sample_gray(row, x, bit_depth);
            over_white(Rgb::new(v, v, v), 255)
        }
        4 => {
            let i = x * channels;
            let v = row.get(i).copied().unwrap_or(0);
            let a = row.get(i + 1).copied().unwrap_or(255);
            over_white(Rgb::new(v, v, v), a)
        }
        2 => {
            let i = x * channels;
            Rgb::new(
                row.get(i).copied().unwrap_or(0),
                row.get(i + 1).copied().unwrap_or(0),
                row.get(i + 2).copied().unwrap_or(0),
            )
        }
        _ => {
            let i = x * channels;
            let c = Rgb::new(
                row.get(i).copied().unwrap_or(0),
                row.get(i + 1).copied().unwrap_or(0),
                row.get(i + 2).copied().unwrap_or(0),
            );
            let a = row.get(i + 3).copied().unwrap_or(255);
            over_white(c, a)
        }
    }
}

/// 与白底合成。透明像素在预览里看到的通常是白底，跟着这个直觉走最不容易出戏。
fn over_white(c: Rgb, a: u8) -> Rgb {
    if a == 255 {
        return c;
    }
    let af = a as f32 / 255.0;
    let mix = |v: u8| {
        (v as f32 * af + 255.0 * (1.0 - af))
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Rgb::new(mix(c.r), mix(c.g), mix(c.b))
}

fn sample_index(row: &[u8], x: usize, bit_depth: u8) -> usize {
    match bit_depth {
        8 => row.get(x).copied().unwrap_or(0) as usize,
        4 => {
            let b = row.get(x / 2).copied().unwrap_or(0);
            (if x.is_multiple_of(2) {
                b >> 4
            } else {
                b & 0x0f
            }) as usize
        }
        2 => {
            let b = row.get(x / 4).copied().unwrap_or(0);
            ((b >> (6 - 2 * (x % 4))) & 0x03) as usize
        }
        1 => {
            let b = row.get(x / 8).copied().unwrap_or(0);
            ((b >> (7 - (x % 8))) & 0x01) as usize
        }
        _ => 0,
    }
}

fn sample_gray(row: &[u8], x: usize, bit_depth: u8) -> u8 {
    match bit_depth {
        8 => row.get(x).copied().unwrap_or(0),
        4 => {
            let v = sample_index(row, x, 4) as u8;
            v * 17
        }
        2 => {
            let v = sample_index(row, x, 2) as u8;
            v * 85
        }
        1 => {
            let v = sample_index(row, x, 1) as u8;
            v * 255
        }
        _ => 0,
    }
}

// --------------------------------------------------------------------- BMP

fn bmp_dimensions(data: &[u8]) -> Option<(usize, usize)> {
    let w = i32::from_le_bytes(data.get(18..22)?.try_into().ok()?);
    let h = i32::from_le_bytes(data.get(22..26)?.try_into().ok()?);
    if w <= 0 || h == 0 {
        return None;
    }
    Some((w as usize, h.unsigned_abs() as usize))
}

pub(crate) fn decode_bmp(data: &[u8]) -> Result<Bitmap, DecodeError> {
    if data.len() < 54 {
        return Err(DecodeError::Truncated);
    }
    let data_offset = u32::from_le_bytes(data[10..14].try_into().unwrap()) as usize;
    let header_size = u32::from_le_bytes(data[14..18].try_into().unwrap()) as usize;
    let width = i32::from_le_bytes(data[18..22].try_into().unwrap());
    let height = i32::from_le_bytes(data[22..26].try_into().unwrap());
    let planes = u16::from_le_bytes(data[26..28].try_into().unwrap());
    let bpp = u16::from_le_bytes(data[28..30].try_into().unwrap());
    let compression = u32::from_le_bytes(data[30..34].try_into().unwrap());

    if header_size < 40 {
        return Err(DecodeError::Unsupported("旧版 BITMAPCOREHEADER".into()));
    }
    if width <= 0 || height == 0 {
        return Err(DecodeError::Corrupt("BMP 尺寸非法".into()));
    }
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    if w > MAX_DIM || h > MAX_DIM {
        return Err(DecodeError::Corrupt(format!("尺寸不合理：{w}×{h}")));
    }
    if planes != 1 {
        return Err(DecodeError::Unsupported("多平面 BMP".into()));
    }
    // BI_RGB=0 / BI_BITFIELDS=3；其余（RLE 等）不支持
    if compression != 0 && compression != 3 {
        return Err(DecodeError::Unsupported(format!(
            "带压缩的 BMP（compression={compression}）"
        )));
    }
    if !matches!(bpp, 8 | 24 | 32) {
        return Err(DecodeError::Unsupported(format!("{bpp} 位色的 BMP")));
    }

    let palette: Vec<Rgb> = if bpp == 8 {
        let mut p = Vec::new();
        let base = 14 + header_size;
        for i in 0..256 {
            let off = base + i * 4;
            let b = data.get(off).copied().unwrap_or(0);
            let g = data.get(off + 1).copied().unwrap_or(0);
            let r = data.get(off + 2).copied().unwrap_or(0);
            p.push(Rgb::new(r, g, b));
        }
        p
    } else {
        Vec::new()
    };

    let bytes_per_px = (bpp / 8) as usize;
    let row_stride = (w * bytes_per_px).div_ceil(4) * 4;
    // 高度为正 = 自下而上存储
    let bottom_up = height > 0;
    let mut bmp = Bitmap::new(w, h);
    let mut offset = data_offset;

    for row in 0..h {
        let y = if bottom_up { h - 1 - row } else { row };
        for x in 0..w {
            let i = offset + x * bytes_per_px;
            let px = match bpp {
                8 => {
                    let idx = data.get(i).copied().unwrap_or(0) as usize;
                    palette.get(idx).copied().unwrap_or_default()
                }
                24 => Rgb::new(
                    data.get(i + 2).copied().unwrap_or(0),
                    data.get(i + 1).copied().unwrap_or(0),
                    data.get(i).copied().unwrap_or(0),
                ),
                _ => Rgb::new(
                    data.get(i + 2).copied().unwrap_or(0),
                    data.get(i + 1).copied().unwrap_or(0),
                    data.get(i).copied().unwrap_or(0),
                ),
            };
            bmp.set(x, y, px);
        }
        offset = offset.saturating_add(row_stride);
        if offset >= data.len() {
            // 数据不够就停下，已经解出的部分仍然有用
            break;
        }
    }
    Ok(bmp)
}

// -------------------------------------------------------------------- JPEG

/// 从 JPEG 的 SOFn 段读尺寸。
pub(crate) fn jpeg_dimensions(data: &[u8]) -> Option<(usize, usize)> {
    let mut pos = 2usize;
    while pos + 4 <= data.len() {
        if data[pos] != 0xff {
            pos += 1;
            continue;
        }
        let marker = data[pos + 1];
        // 填充字节
        if marker == 0xff {
            pos += 1;
            continue;
        }
        // 无长度字段的标记
        if matches!(marker, 0xd8 | 0x01) || (0xd0..=0xd7).contains(&marker) {
            pos += 2;
            continue;
        }
        // 到扫描数据就结束（尺寸信息一定在 SOS 之前）
        if marker == 0xda {
            break;
        }
        let len = be16(&data[pos + 2..])? as usize;
        if len < 2 {
            return None;
        }
        // SOF0..SOF15，排除 DHT(c4) / JPG(c8) / DAC(cc)
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let h = be16(&data[pos + 5..])? as usize;
            let w = be16(&data[pos + 7..])? as usize;
            if w == 0 || h == 0 {
                return None;
            }
            return Some((w, h));
        }
        pos += 2 + len;
    }
    None
}

/// 读 EXIF 里的方向标记（1..8）。手机竖拍的照片会靠它摆正。
pub(crate) fn jpeg_orientation(data: &[u8]) -> Option<u8> {
    let mut pos = 2usize;
    while pos + 4 <= data.len() {
        if data[pos] != 0xff {
            pos += 1;
            continue;
        }
        let marker = data[pos + 1];
        if marker == 0xff {
            pos += 1;
            continue;
        }
        if matches!(marker, 0xd8 | 0x01) || (0xd0..=0xd7).contains(&marker) {
            pos += 2;
            continue;
        }
        if marker == 0xda {
            break;
        }
        let len = be16(&data[pos + 2..])? as usize;
        if marker == 0xe1 && len >= 8 {
            let seg = data.get(pos + 4..pos + 2 + len)?;
            if seg.starts_with(b"Exif\0\0") {
                if let Some(o) = parse_exif_orientation(&seg[6..]) {
                    return Some(o);
                }
            }
        }
        pos += 2 + len;
    }
    None
}

fn parse_exif_orientation(tiff: &[u8]) -> Option<u8> {
    let big = match tiff.get(0..2)? {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let rd16 = |b: &[u8]| -> Option<u16> {
        let arr: [u8; 2] = b.get(0..2)?.try_into().ok()?;
        Some(if big {
            u16::from_be_bytes(arr)
        } else {
            u16::from_le_bytes(arr)
        })
    };
    let rd32 = |b: &[u8]| -> Option<u32> {
        let arr: [u8; 4] = b.get(0..4)?.try_into().ok()?;
        Some(if big {
            u32::from_be_bytes(arr)
        } else {
            u32::from_le_bytes(arr)
        })
    };
    if rd16(&tiff[2..])? != 42 {
        return None;
    }
    let ifd0 = rd32(&tiff[4..])? as usize;
    let count = rd16(tiff.get(ifd0..)?)? as usize;
    for i in 0..count.min(64) {
        let entry = ifd0 + 2 + i * 12;
        let tag = rd16(tiff.get(entry..)?)?;
        if tag == 0x0112 {
            let value = rd16(tiff.get(entry + 8..)?)?;
            if (1..=8).contains(&value) {
                return Some(value as u8);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用 Python 的 zlib 手工拼一张 2×2 的真彩 PNG（无过滤）。
    ///
    /// 手写字节而不是塞一个二进制 fixture：这样一眼能看出每段是什么，
    /// 出了问题也知道该去看哪。
    fn tiny_png() -> Vec<u8> {
        // 2×2 像素，颜色分别是 红 / 绿 / 蓝 / 白
        let raw_pixels: Vec<u8> = vec![
            0xff, 0x00, 0x00, 0x00, 0xff, 0x00, // 第 1 行：红、绿
            0x00, 0x00, 0xff, 0xff, 0xff, 0xff, // 第 2 行：蓝、白
        ];
        // 每行前面加一个 filter 字节（0 = None）
        let mut raw = Vec::new();
        raw.push(0);
        raw.extend_from_slice(&raw_pixels[..6]);
        raw.push(0);
        raw.extend_from_slice(&raw_pixels[6..]);

        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        // IHDR
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&2u32.to_be_bytes()); // width
        ihdr.extend_from_slice(&2u32.to_be_bytes()); // height
        ihdr.push(8); // bit depth
        ihdr.push(2); // color type = RGB
        ihdr.push(0);
        ihdr.push(0);
        ihdr.push(0);
        push_chunk(&mut png, b"IHDR", &ihdr);
        push_chunk(&mut png, b"IDAT", &zlib_store(&raw));
        push_chunk(&mut png, b"IEND", &[]);
        png
    }

    /// 生成一个 **stored 块**的 zlib 流（不依赖 zlib，纯手拼）。
    fn zlib_store(raw: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let mut i = 0usize;
        while i < raw.len() {
            let n = (raw.len() - i).min(0xffff);
            let final_block = i + n >= raw.len();
            out.push(if final_block { 0x01 } else { 0x00 });
            out.extend_from_slice(&(n as u16).to_le_bytes());
            out.extend_from_slice(&(!(n as u16)).to_le_bytes());
            out.extend_from_slice(&raw[i..i + n]);
            i += n;
        }
        if raw.is_empty() {
            out.push(0x01);
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0xffffu16.to_le_bytes());
        }
        // adler32 不校验，随便填
        out.extend_from_slice(&0u32.to_be_bytes());
        out
    }

    fn push_chunk(png: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
        png.extend_from_slice(&(body.len() as u32).to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(body);
        png.extend_from_slice(&0u32.to_be_bytes()); // CRC 不校验
    }

    #[test]
    fn sniffing_trusts_magic_not_names() {
        assert_eq!(sniff(&tiny_png()), Some(Format::Png));
        assert_eq!(sniff(b"BM____________"), Some(Format::Bmp));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some(Format::Jpeg));
        assert_eq!(sniff(b"GIF89a"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn decodes_a_hand_built_png() {
        let bmp = decode_png(&tiny_png()).unwrap();
        assert_eq!((bmp.width, bmp.height), (2, 2));
        assert_eq!(bmp.get(0, 0), Rgb::new(0xff, 0x00, 0x00));
        assert_eq!(bmp.get(1, 0), Rgb::new(0x00, 0xff, 0x00));
        assert_eq!(bmp.get(0, 1), Rgb::new(0x00, 0x00, 0xff));
        assert_eq!(bmp.get(1, 1), Rgb::new(0xff, 0xff, 0xff));
    }

    #[test]
    fn png_dimensions_can_be_read_without_decoding() {
        assert_eq!(png_dimensions(&tiny_png()), Some((2, 2)));
    }

    #[test]
    fn paeth_predictor_picks_the_closest_neighbour() {
        // 规范里的经典用例
        assert_eq!(paeth(10, 20, 15), 15);
        assert_eq!(paeth(0, 0, 0), 0);
        assert_eq!(paeth(255, 0, 0), 255);
        assert_eq!(paeth(0, 255, 0), 255);
    }

    #[test]
    fn unfilter_reverses_every_filter_type() {
        // Sub：每个字节加左邻
        let mut cur = vec![10u8, 5, 5];
        unfilter(1, 1, &mut cur, &[0, 0, 0]).unwrap();
        assert_eq!(cur, vec![10, 15, 20]);

        // Up：加上一行
        let mut cur = vec![1u8, 2, 3];
        unfilter(2, 1, &mut cur, &[10, 20, 30]).unwrap();
        assert_eq!(cur, vec![11, 22, 33]);

        // None 不动
        let mut cur = vec![7u8, 8];
        unfilter(0, 1, &mut cur, &[1, 1]).unwrap();
        assert_eq!(cur, vec![7, 8]);

        assert!(unfilter(9, 1, &mut [0u8, 1], &[0, 0]).is_err());
    }

    #[test]
    fn jpeg_dimensions_are_read_from_sof0() {
        // SOI + APP0(JFIF) + SOF0(3 分量, 高 480 宽 640)
        let mut j = vec![0xff, 0xd8];
        j.extend_from_slice(&[0xff, 0xe0, 0x00, 0x10]);
        j.extend_from_slice(b"JFIF\0");
        j.extend_from_slice(&[0u8; 9]); // 补足 16 字节段长
        j.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]);
        j.extend_from_slice(&480u16.to_be_bytes());
        j.extend_from_slice(&640u16.to_be_bytes());
        j.push(3);
        j.extend_from_slice(&[0u8; 9]);
        assert_eq!(jpeg_dimensions(&j), Some((640, 480)));
    }

    /// 造一个只有帧头的 JPEG：SOI + 一张量化表 + 帧头，后面不带 DHT/SOS。
    /// 灰度、8 位、1×1 采样，所以帧头一定解析得过去——这样后面报出来的
    /// 错误就只可能来自「数据到此为止」或「这个变体不支持」。
    fn jpeg_with_frame_header(marker: u8, w: u16, h: u16) -> Vec<u8> {
        let mut j = vec![0xff, 0xd8];
        // DQT：编号 0 的 8 位量化表，值全填 16
        j.extend_from_slice(&[0xff, 0xdb, 0x00, 0x43, 0x00]);
        j.extend_from_slice(&[16u8; 64]);
        // 帧头：精度 8、高、宽、1 个分量(id=1, h=1 v=1, 量化表 0)
        j.extend_from_slice(&[0xff, marker, 0x00, 0x0b, 0x08]);
        j.extend_from_slice(&h.to_be_bytes());
        j.extend_from_slice(&w.to_be_bytes());
        j.push(1);
        j.extend_from_slice(&[0x01, 0x11, 0x00]);
        j
    }

    /// 基线 JPEG 从这一步开始是**真能解出像素**的（见 `jpeg.rs` 与
    /// `tests/jpeg_reference.rs`），所以这个测试只负责两件事：
    /// 残片要**大声报错**、尺寸无论如何都读得出来。
    #[test]
    fn a_half_jpeg_fails_loudly_while_its_size_is_still_readable() {
        // 只有 SOI + SOF0，没有量化表 / 霍夫曼表 / 扫描段。
        let j = jpeg_with_frame_header(0xc0, 200, 100);
        assert_eq!(dimensions(&j), Some((200, 100)));
        assert!(
            matches!(
                decode(&j),
                Err(DecodeError::Truncated) | Err(DecodeError::Corrupt(_))
            ),
            "半张 JPEG 不能悄悄解出点什么：{:?}",
            decode(&j)
        );
    }

    /// 渐进式是**认得出但不支持**，理由要说准（不能张冠李戴成「扫描段跑到了帧头前面」）。
    #[test]
    fn progressive_jpeg_is_recognised_and_refused_with_a_precise_reason() {
        let j = jpeg_with_frame_header(0xc2, 200, 100);
        assert_eq!(
            dimensions(&j),
            Some((200, 100)),
            "尺寸信息在帧头里，读得出来"
        );
        match decode(&j) {
            Err(DecodeError::Unsupported(why)) => {
                assert!(
                    why.contains("渐进式"),
                    "理由要指出是渐进式，实际是「{why}」"
                );
            }
            other => panic!("渐进式应当报 Unsupported，实际 {other:?}"),
        }
    }

    /// 垃圾数据既不能 panic，也不能被当成「一张纯黑的图」。
    #[test]
    fn garbage_after_the_jpeg_magic_is_rejected_rather_than_guessed() {
        let j = b"\xff\xd8\xff\xd8\xff\xd8\xff\xd8".to_vec();
        assert!(decode(&j).is_err());
    }

    #[test]
    fn a_bmp_roundtrip_works_bottom_up() {
        // 2×2、24 位、自下而上
        let w = 2usize;
        let h = 2usize;
        let stride = (w * 3).div_ceil(4) * 4; // 8
        let mut bmp = Vec::new();
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes()); // pixel offset
        bmp.extend_from_slice(&40u32.to_le_bytes()); // header size
        bmp.extend_from_slice(&(w as i32).to_le_bytes());
        bmp.extend_from_slice(&(h as i32).to_le_bytes()); // 正数 = 自下而上
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        let data_size: u32 = (stride * h) as u32;
        bmp.extend_from_slice(&data_size.to_le_bytes());
        bmp.extend_from_slice(&[0u8; 16]);
        // 第一行数据 = 图像的最底行
        bmp.extend_from_slice(&[0x00, 0x00, 0xff, 0x00, 0xff, 0x00, 0x00, 0x00]); // 红, 绿
        bmp.extend_from_slice(&[0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00]); // 白, 黑

        let out = decode_bmp(&bmp).unwrap();
        assert_eq!((out.width, out.height), (2, 2));
        // 最底行是 红/绿，最顶行是 白/黑
        assert_eq!(out.get(0, 1), Rgb::new(0xff, 0x00, 0x00));
        assert_eq!(out.get(1, 1), Rgb::new(0x00, 0xff, 0x00));
        assert_eq!(out.get(0, 0), Rgb::new(0xff, 0xff, 0xff));
        assert_eq!(out.get(1, 0), Rgb::new(0x00, 0x00, 0x00));
        assert_eq!(bmp_dimensions(&bmp), Some((2, 2)));
    }

    #[test]
    fn colour_helpers_behave() {
        assert!(Rgb::new(255, 255, 255).luma() > 0.99);
        assert!(Rgb::new(0, 0, 0).luma() < 0.01);
        assert_eq!(Rgb::new(255, 0, 0).hex(), "#ff0000");
        assert!(Rgb::new(255, 0, 0).saturation() > 0.99);
        assert!(Rgb::new(128, 128, 128).saturation() < 0.01);
        assert!(Rgb::new(255, 0, 0).hue() < 1.0);
        assert!((Rgb::new(0, 255, 0).hue() - 120.0).abs() < 1.0);
        assert!((Rgb::new(0, 0, 255).hue() - 240.0).abs() < 1.0);
        assert_eq!(Rgb::new(10, 10, 10).hue(), 0.0, "灰色没有色相");
    }

    #[test]
    fn alpha_is_composited_over_white() {
        // 全透明的红色 → 白
        assert_eq!(over_white(Rgb::new(255, 0, 0), 0), Rgb::new(255, 255, 255));
        // 不透明的红色 → 红
        assert_eq!(over_white(Rgb::new(255, 0, 0), 255), Rgb::new(255, 0, 0));
        // 半透明 → 中间色
        let mid = over_white(Rgb::new(0, 0, 0), 128);
        assert!((120..=136).contains(&mid.r), "{:?}", mid);
    }

    #[test]
    fn truncated_and_foreign_data_are_rejected_cleanly() {
        assert!(matches!(
            decode(b"not an image"),
            Err(DecodeError::NotAnImage)
        ));
        assert!(matches!(decode(&[]), Err(DecodeError::NotAnImage)));
        let mut short_png = tiny_png();
        short_png.truncate(30);
        assert!(decode_png(&short_png).is_err());
    }
}
