//! 内存访问的地址对齐与字节 lane 处理
//!
//! SWD AHB-AP 只支持自然对齐的访问：对非字对齐地址发起 32 位 DRW 访问
//! 行为不可预测（实际芯片通常回退到对齐字）。因此子字变量（u8/u16）必须
//! 读取其所在的对齐整字，再按字节 lane 提取；写入则需读-改-写。

/// 一次子字/整字内存访问的对齐信息
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemAccess {
    /// 对齐后的字地址（实际写入 TAR 的值）
    pub aligned_addr: u32,
    /// 变量在整字中的位移（字节 ×8）
    pub shift: u32,
    /// 变量在整字中的掩码
    pub mask: u32,
}

impl MemAccess {
    /// 按变量地址与字节大小（1/2/4，其他按 4 处理）推导对齐访问信息
    pub fn from_addr_size(addr: u32, byte_size: u32) -> Self {
        match byte_size {
            1 => {
                let shift = (addr & 3) * 8;
                Self { aligned_addr: addr & !3, shift, mask: 0xFFu32 << shift }
            }
            2 => {
                let shift = (addr & 2) * 8;
                Self { aligned_addr: addr & !3, shift, mask: 0xFFFFu32 << shift }
            }
            _ => Self { aligned_addr: addr, shift: 0, mask: u32::MAX },
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

    #[test]
    fn merge_value_truncated_to_lane() {
        // 写入值超出变量宽度时按 lane 截断（watch 输入按类型解析，防御性）
        let acc = MemAccess::from_addr_size(0x2000_0002, 2);
        assert_eq!(acc.merge(0x2222_1111, 0x1_0005), 0x0005_1111);
    }
}
