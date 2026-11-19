//! 内存访问的地址对齐与字节 lane 处理
//!
//! SWD AHB-AP 只支持自然对齐的访问：对非字对齐地址发起 32 位 DRW 访问
//! 行为不可预测（实际芯片通常回退到对齐字）。因此子字变量（u8/u16）必须
//! 读取其所在的对齐整字，再按字节 lane 提取；写入则需读-改-写。
//!
//! 8 字节类型（double/int64/uint64）由两个相邻 32 位字组成：AHB-AP 没有
//! 64 位访问，引擎发出两次对齐字读，按**小端**拼成 u64（目标恒为 Cortex-M，
//! 只有小端；拼接是数值移位运算，与宿主机字节序无关）。

/// 变量在内存中的访问布局
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemAccess {
    /// 首字的对齐地址（实际写入 TAR 的值）
    pub aligned_addr: u32,
    /// 变量在首字中的位移（字节 ×8；64 位类型恒为 0）
    pub shift: u32,
    /// 变量在首字中的掩码
    pub mask: u32,
    /// 需要读取的 32 位字数（1..=2；8 字节类型未对齐时为 3）
    pub words: u8,
}

impl MemAccess {
    /// 按变量地址与字节大小（1/2/4/8）推导访问布局
    pub fn from_addr_size(addr: u32, byte_size: u32) -> Self {
        match byte_size {
            1 => {
                let shift = (addr & 3) * 8;
                Self { aligned_addr: addr & !3, shift, mask: 0xFFu32 << shift, words: 1 }
            }
            2 => {
                let shift = (addr & 2) * 8;
                Self { aligned_addr: addr & !3, shift, mask: 0xFFFFu32 << shift, words: 1 }
            }
            8 => {
                // 自然对齐的 64 位变量恰占两个整字；未对齐时跨 3 字
                let words = if addr & 7 == 0 { 2 } else { 3 };
                Self { aligned_addr: addr & !3, shift: 0, mask: u32::MAX, words }
            }
            _ => Self { aligned_addr: addr, shift: 0, mask: u32::MAX, words: 1 },
        }
    }

    /// 从对齐整字中提取变量值（低位对齐到 u32 低端）
    pub fn extract(&self, word: u32) -> u32 {
        (word & self.mask) >> self.shift
    }

    /// 读-改-写：把变量值（低位对齐）合并进当前整字
    pub fn merge(&self, word: u32, value: u32) -> u32 {
        (word & !self.mask) | ((value << self.shift) & self.mask)
    }

    /// 把连续读回的 32 位字拼成变量的原始位模式
    ///
    /// `words` 为按地址递增顺序读回的字。32 位及更窄类型取首字 lane 提取；
    /// 64 位类型按小端拼成 u64（低字在前）。
    pub fn extract_wide(&self, words: &[u32]) -> u64 {
        if self.words == 1 {
            return self.extract(words.first().copied().unwrap_or(0)) as u64;
        }
        let low = words.first().copied().unwrap_or(0);
        let high = words.get(1).copied().unwrap_or(0);
        u64::from(low) | (u64::from(high) << 32)
    }

    /// 把 64 位值拆成 (低字, 高字) 供两次 DRW 写
    pub fn split_wide(value: u64) -> (u32, u32) {
        (value as u32, (value >> 32) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- extract：读路径 lane 提取 ----

    #[test]
    fn u16_at_offset2_extracts_high_lane() {
        // 核心 bug 复现：内存 0x1111@+0 (a) / 0x2222@+2 (b)，
        // 字对齐读 0x20000000 得 0x22221111，b 必须取高 16 位 0x2222，
        // 而不是低 16 位（a 的值）——"两两重叠" bug。
        let acc = MemAccess::from_addr_size(0x2000_0002, 2);
        assert_eq!(acc.aligned_addr, 0x2000_0000);
        assert_eq!(acc.extract(0x2222_1111), 0x2222);
    }

    #[test]
    fn u16_at_offset0_extracts_low_lane() {
        let acc = MemAccess::from_addr_size(0x2000_0000, 2);
        assert_eq!(acc.aligned_addr, 0x2000_0000);
        assert_eq!(acc.extract(0x2222_1111), 0x1111);
    }

    #[test]
    fn u32_passthrough() {
        let acc = MemAccess::from_addr_size(0x2000_0104, 4);
        assert_eq!(acc.aligned_addr, 0x2000_0104);
        assert_eq!(acc.extract(0xABCD_1234), 0xABCD_1234);
    }

    #[test]
    fn u8_lanes() {
        let w = 0x4433_2211;
        assert_eq!(MemAccess::from_addr_size(0x2000_0000, 1).extract(w), 0x11);
        assert_eq!(MemAccess::from_addr_size(0x2000_0001, 1).extract(w), 0x22);
        assert_eq!(MemAccess::from_addr_size(0x2000_0002, 1).extract(w), 0x33);
        assert_eq!(MemAccess::from_addr_size(0x2000_0003, 1).extract(w), 0x44);
        // u8@+1 的 TAR 应对齐到字首
        assert_eq!(MemAccess::from_addr_size(0x2000_0001, 1).aligned_addr, 0x2000_0000);
    }

    // ---- merge：写路径读-改-写 ----

    #[test]
    fn merge_u16_at_offset2_preserves_low_half() {
        // 向 b（@+2）写 5：a 的 0x1111 必须保留，高 16 位变 0x0005
        let acc = MemAccess::from_addr_size(0x2000_0002, 2);
        assert_eq!(acc.merge(0x2222_1111, 5), 0x0005_1111);
    }

    #[test]
    fn merge_u16_at_offset0_preserves_high_half() {
        let acc = MemAccess::from_addr_size(0x2000_0000, 2);
        assert_eq!(acc.merge(0x2222_1111, 5), 0x2222_0005);
    }

    #[test]
    fn merge_u8_at_offset3() {
        let acc = MemAccess::from_addr_size(0x2000_0003, 1);
        assert_eq!(acc.merge(0x4433_2211, 0xAB), 0xAB33_2211);
    }

    #[test]
    fn merge_u32_replaces_whole_word() {
        let acc = MemAccess::from_addr_size(0x2000_0000, 4);
        assert_eq!(acc.merge(0x2222_1111, 0xABCD_1234), 0xABCD_1234);
    }

    // ---- 64 位类型：双字访问 ----

    #[test]
    fn wide_types_span_two_words() {
        let acc = MemAccess::from_addr_size(0x2000_0000, 8);
        assert_eq!(acc.words, 2, "aligned 64-bit variable occupies 2 words");
        assert_eq!(acc.aligned_addr, 0x2000_0000);
        // 仅 4 字节对齐（非 8 对齐）的 64 位变量跨 3 个整字
        let acc = MemAccess::from_addr_size(0x2000_0004, 8);
        assert_eq!(acc.words, 3, "misaligned 64-bit value spans 3 words");
        assert_eq!(acc.aligned_addr, 0x2000_0004);
    }

    #[test]
    fn extract_wide_assembles_little_endian_u64() {
        // double 1.5 的位模式 0x3FF8000000000000 -> 低字 0, 高字 0x3FF80000
        let acc = MemAccess::from_addr_size(0x2000_0000, 8);
        let value = 1.5f64.to_bits();
        let (low, high) = MemAccess::split_wide(value);
        assert_eq!(acc.extract_wide(&[low, high]), value);
        // 高字必须放在 u64 高位：拼错会得到完全不同的浮点值
        assert_ne!(acc.extract_wide(&[high, low]), value);
    }

    #[test]
    fn extract_wide_handles_32bit_types() {
        let acc = MemAccess::from_addr_size(0x2000_0000, 4);
        assert_eq!(acc.words, 1);
        assert_eq!(acc.extract_wide(&[0xABCD_1234]), 0xABCD_1234);
        // 子字类型仍按 lane 提取
        let acc = MemAccess::from_addr_size(0x2000_0002, 2);
        assert_eq!(acc.extract_wide(&[0x2222_1111]), 0x2222);
    }

    #[test]
    fn merge_value_truncated_to_lane() {
        // 写入值超出变量宽度时按 lane 截断（watch 输入按类型解析，防御性）
        let acc = MemAccess::from_addr_size(0x2000_0002, 2);
        assert_eq!(acc.merge(0x2222_1111, 0x1_0005), 0x0005_1111);
    }
}
