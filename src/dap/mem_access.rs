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
    /// 变量在首字中的位移（字节 ×8）
    ///
    /// 1/2 字节类型按 lane 提取；8 字节类型只有在地址不是 4 的倍数时
    /// 才非零（跨三个对齐字）。
    pub shift: u32,
    /// 变量在首字中的掩码
    pub mask: u32,
    /// 需要读取的 32 位字数（1..=3）
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
                // AHB-AP 以 32 位字为单位访问，8 字节变量按地址对齐情况
                // 占 2 或 3 个对齐字：
                // - 4 字节对齐（含 8 字节对齐）：恰好占 2 个整字
                // - 非 4 字节对齐：首尾字与相邻变量共享，跨 3 个字，
                //   变量值在拼接后还需右移字内位移
                if addr & 3 == 0 {
                    Self { aligned_addr: addr, shift: 0, mask: u32::MAX, words: 2 }
                } else {
                    let shift = (addr & 3) * 8;
                    Self { aligned_addr: addr & !3, shift, mask: u32::MAX, words: 3 }
                }
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
    /// 64 位类型按小端拼成 u64（低字在前），跨 3 字时再按字内位移右移。
    pub fn extract_wide(&self, words: &[u32]) -> u64 {
        if self.words == 1 {
            return self.extract(words.first().copied().unwrap_or(0)) as u64;
        }
        let mut acc: u128 = 0;
        for (i, &w) in words.iter().take(self.words as usize).enumerate() {
            acc |= u128::from(w) << (32 * i);
        }
        // 3 字情形（非 4 字节对齐）变量起始于首字内的字节位移处，
        // 右移去掉前面的填充字节；取低 64 位即变量值
        ((acc >> self.shift) & u128::from(u64::MAX)) as u64
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
    fn wide_types_span_two_or_three_words() {
        // 4 字节对齐（含 8 字节对齐）的 64 位变量恰占两个整字
        let acc = MemAccess::from_addr_size(0x2000_0000, 8);
        assert_eq!(acc.words, 2, "aligned 64-bit variable occupies 2 words");
        assert_eq!(acc.aligned_addr, 0x2000_0000);
        assert_eq!(acc.shift, 0);
        let acc = MemAccess::from_addr_size(0x2000_0004, 8);
        assert_eq!(acc.words, 2, "4-byte aligned still fits two words");
        assert_eq!(acc.aligned_addr, 0x2000_0004);
        // 非 4 字节对齐的 64 位变量跨 3 个整字，且带字内位移
        let acc = MemAccess::from_addr_size(0x2000_0002, 8);
        assert_eq!(acc.words, 3, "misaligned 64-bit value spans 3 words");
        assert_eq!(acc.aligned_addr, 0x2000_0000);
        assert_eq!(acc.shift, 16);
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
    fn extract_wide_misaligned_three_words_shifts_correctly() {
        // 变量起始于 +2：窗口 = [pad16 | v0..16] [v16..48] [v48..64 | pad16]
        let value = 0x1122_3344_5566_7788u64;
        let acc = MemAccess::from_addr_size(0x2000_0002, 8);
        let w0 = ((value as u32 as u16 as u32) << 16) | 0xAAAA; // 低 16 位是前置变量
        let w1 = (value >> 16) as u32; // 中间 32 位
        let w2 = (((value >> 48) as u32) & 0xFFFF) | (0xDEAD << 16); // 后置变量
        assert_eq!(acc.extract_wide(&[w0, w1, w2]), value);
        // 只给 2 个字（FAULT 截断）时不应误当成完整值
        assert_ne!(acc.extract_wide(&[w0, w1]), value);
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
