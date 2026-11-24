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
    /// 1/2 字节类型按 lane 提取；4 字节类型仅当地址不是 4 的倍数时非零
    /// （跨两个对齐字）；8 字节类型只有在地址不是 4 的倍数时才非零
    /// （跨三个对齐字）。
    pub shift: u32,
    /// 变量在首字中的掩码（仅 1/2 字节类型用于读-改-写）
    pub mask: u32,
    /// 需要读取的 32 位字数（1..=3）
    pub words: u8,
    /// 变量的字节大小（1/2/4/8）
    ///
    /// 写入路径必须按它区分类型语义：仅凭 `words` 无法区分「非对齐
    /// 32 位（占 2 字）」与「64 位（占 2 字）」——按 words 判断会把
    /// 非对齐 u32 误当成 u64 拆分写入，破坏相邻内存。
    pub byte_size: u32,
}

impl MemAccess {
    /// 按变量地址与字节大小（1/2/4/8）推导访问布局
    pub fn from_addr_size(addr: u32, byte_size: u32) -> Self {
        match byte_size {
            1 => {
                let shift = (addr & 3) * 8;
                Self {
                    aligned_addr: addr & !3,
                    shift,
                    mask: 0xFFu32 << shift,
                    words: 1,
                    byte_size,
                }
            }
            2 => {
                let shift = (addr & 2) * 8;
                Self {
                    aligned_addr: addr & !3,
                    shift,
                    mask: 0xFFFFu32 << shift,
                    words: 1,
                    byte_size,
                }
            }
            4 => {
                // AHB-AP 不支持非对齐访问：非 4 字节对齐的 u32/float
                // 横跨两个对齐字，必须读回两个字后再右移拼接
                // （packed struct 的成员很常见，不能直接下发非对齐地址）
                if addr & 3 == 0 {
                    Self {
                        aligned_addr: addr,
                        shift: 0,
                        mask: u32::MAX,
                        words: 1,
                        byte_size,
                    }
                } else {
                    Self {
                        aligned_addr: addr & !3,
                        shift: (addr & 3) * 8,
                        mask: u32::MAX,
                        words: 2,
                        byte_size,
                    }
                }
            }
            8 => {
                // AHB-AP 以 32 位字为单位访问，8 字节变量按地址对齐情况
                // 占 2 或 3 个对齐字：
                // - 4 字节对齐（含 8 字节对齐）：恰好占 2 个整字
                // - 非 4 字节对齐：首尾字与相邻变量共享，跨 3 个字，
                //   变量值在拼接后还需右移字内位移
                if addr & 3 == 0 {
                    Self {
                        aligned_addr: addr,
                        shift: 0,
                        mask: u32::MAX,
                        words: 2,
                        byte_size,
                    }
                } else {
                    let shift = (addr & 3) * 8;
                    Self {
                        aligned_addr: addr & !3,
                        shift,
                        mask: u32::MAX,
                        words: 3,
                        byte_size,
                    }
                }
            }
            _ => Self {
                aligned_addr: addr,
                shift: 0,
                mask: u32::MAX,
                words: 1,
                byte_size: 4,
            },
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

    /// 读-改-写：把变量值按字节写进对齐窗口，其余字节保持窗口原值
    ///
    /// 适用于任意宽度与字内位移：
    /// - 子字变量（u8/u16）：只改写窗口首字中的对应字节 lane
    /// - 非对齐 32 位变量：跨两个对齐字，逐字节拼接
    /// - 非对齐 64 位变量：跨三个对齐字，逐字节拼接
    ///
    /// `window` 长度必须 ≥ [`MemAccess::words`]，且内容为最近一次采样
    /// 读回的原始对齐字。调用方负责保证窗口有效（未采到旧值时不得写）。
    pub fn splice_into(&self, window: &mut [u32], value: u64) {
        let src = value.to_le_bytes();
        let len = (self.byte_size as usize).min(8);
        let byte_off = (self.shift / 8) as usize;
        for (i, &byte) in src.iter().take(len).enumerate() {
            let pos = byte_off + i;
            let word_idx = pos / 4;
            let byte_in_word = pos % 4;
            let Some(word) = window.get_mut(word_idx) else {
                break;
            };
            let shift = (byte_in_word * 8) as u32;
            *word = (*word & !(0xFFu32 << shift)) | ((byte as u32) << shift);
        }
    }

    /// 把连续读回的 32 位字拼成变量的原始位模式
    ///
    /// `words` 为按地址递增顺序读回的字。32 位及更窄类型取首字 lane 提取；
    /// 4/8 字节类型按小端拼接（4 字节非对齐时跨两个字、8 字节非对齐时
    /// 跨三个字），拼接后按字内位移右移，最后按变量宽度截断，去掉相邻
    /// 变量占用的高位。
    pub fn extract_wide(&self, words: &[u32]) -> u64 {
        if self.words == 1 {
            return self.extract(words.first().copied().unwrap_or(0)) as u64;
        }
        let mut acc: u128 = 0;
        for (i, &w) in words.iter().take(self.words as usize).enumerate() {
            acc |= u128::from(w) << (32 * i);
        }
        // 非 4 字节对齐：变量起始于首字内的字节位移处，右移去掉前面的
        // 填充字节。拼接结果的高位属于后续变量，必须按变量宽度截断
        // （4 字节非对齐时不清掉高位会让调用方看到相邻变量数据）。
        let bits = self.byte_size.clamp(1, 8) * 8;
        let mask = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
        ((acc >> self.shift) as u64) & mask
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

    // ---- 非对齐 32 位：必须对齐后跨字拼接 ----

    #[test]
    fn unaligned_u32_spans_two_words() {
        // 非 4 字节对齐的 u32/float（packed struct 成员常见）：
        // AHB-AP 不支持非对齐访问，必须读回两个字再拼接
        let acc = MemAccess::from_addr_size(0x2000_0002, 4);
        assert_eq!(acc.aligned_addr, 0x2000_0000);
        assert_eq!(acc.shift, 16);
        assert_eq!(acc.words, 2);
        assert_eq!(acc.byte_size, 4);
    }

    #[test]
    fn unaligned_u32_extracts_correct_bytes() {
        // 内存布局：0x2000_0000: 0x1111_1111, 0x2000_0004: 0x2222_2222
        // 变量 @+2 长度为 4 字节 → 字节 [11][11][22][22]（小端）
        // 期望值 0x2222_1111
        let acc = MemAccess::from_addr_size(0x2000_0002, 4);
        assert_eq!(acc.extract_wide(&[0x1111_1111, 0x2222_2222]), 0x2222_1111);
    }

    #[test]
    fn aligned_u32_still_single_word() {
        let acc = MemAccess::from_addr_size(0x2000_0004, 4);
        assert_eq!(acc.words, 1);
        assert_eq!(acc.shift, 0);
        assert_eq!(acc.aligned_addr, 0x2000_0004);
    }

    #[test]
    fn unaligned_u32_splice_preserves_neighbours() {
        // 向 @+2 的 u32 写 0xDEAD_BEEF：
        // 窗口 [0x1111_1111, 0x2222_2222] →
        // [0x1111_BEEF?] 不对：低 16 位来自新值的低 16 位 0xBEEF
        // w0 = 0x1111_1111 的低 16 位保留(0x1111) + 新值低 16 位(0xBEEF) → 0xBEEF_1111
        // w1 = 新值高 16 位(0xDEAD) + 0x2222_2222 的高 16 位(0x2222) → 0x2222_DEAD
        let acc = MemAccess::from_addr_size(0x2000_0002, 4);
        let mut window = [0x1111_1111u32, 0x2222_2222, 0];
        acc.splice_into(&mut window, 0xDEAD_BEEFu64);
        assert_eq!(window[0], 0xBEEF_1111);
        assert_eq!(window[1], 0x2222_DEAD);
        // 读回必须还原写入值
        assert_eq!(acc.extract_wide(&window[..2]), 0xDEAD_BEEF);
    }

    #[test]
    fn splice_u16_high_lane_preserves_low_half() {
        // 子字写入也必须走同一路径且行为正确
        let acc = MemAccess::from_addr_size(0x2000_0002, 2);
        let mut window = [0x2222_1111u32, 0, 0];
        acc.splice_into(&mut window, 5);
        assert_eq!(window[0], 0x0005_1111);
    }

    #[test]
    fn splice_unaligned_u64_across_three_words() {
        let value = 0x1122_3344_5566_7788u64;
        let acc = MemAccess::from_addr_size(0x2000_0002, 8);
        assert_eq!(acc.words, 3);
        let mut window = [0xAAAA_AAAAu32, 0xBBBB_BBBB, 0xCCCC_CCCC];
        acc.splice_into(&mut window, value);
        // 读回必须还原写入值（中间的相邻字节被精确覆盖）
        assert_eq!(acc.extract_wide(&window[..3]), value);
        // 首字低 16 位与末字高 16 位必须保留
        assert_eq!(window[0] & 0xFFFF, 0xAAAA);
        assert_eq!(window[2] >> 16, 0xCCCC);
    }

    #[test]
    fn byte_size_field_distinguishes_u32_from_u64() {
        // 关键回归：仅凭 words 无法区分「非对齐 32 位（2 字）」与
        // 「对齐 64 位（2 字）」。按 words 判断写入会把非对齐 u32
        // 误当 u64 拆分写入，破坏相邻内存。
        let unaligned_u32 = MemAccess::from_addr_size(0x2000_0002, 4);
        let aligned_u64 = MemAccess::from_addr_size(0x2000_0000, 8);
        assert_eq!(unaligned_u32.words, aligned_u64.words);
        assert_ne!(unaligned_u32.byte_size, aligned_u64.byte_size);
        assert_eq!(unaligned_u32.byte_size, 4);
        assert_eq!(aligned_u64.byte_size, 8);
    }
}
