use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use super::sample::Sample;

/// 单槽位：数据 + 顺序锁发布序号
///
/// `seq` 偶数 = 数据稳定可读；奇数 = 生产者写入中。
/// 生产者写入：seq 置奇 → 写数据 → seq 置偶（Release 发布）；
/// 消费者读取：读 seq（Acquire）→ 读数据 → 再读 seq，两次一致且为偶
/// 才接受，否则重试——保证不会读到撕裂数据。
struct Slot {
    seq: AtomicU64,
    data: UnsafeCell<Sample>,
}

/// Lock-Free SPSC 环形缓冲区（满时覆盖最旧）
///
/// 单生产者（接收线程写入）、单消费者（主线程读出）。
/// 缓冲区满时**覆盖最旧数据**（示波器惯例：永远保留最新样本），
/// 被覆盖丢弃的采样点数通过 overwritten() 暴露，供 UI 提示消费端落后。
///
/// # Safety
/// - push() 仅由接收线程调用，pop_batch() 仅由主线程调用
/// - 满时覆盖最旧槽位可能与消费者读取该槽并发：槽位顺序锁
///   （seq 奇偶 + 二次校验）保证消费者不会读到撕裂数据
/// - head/tail 的 Release/Acquire 内存序保证数据可见性
pub struct RingBuffer {
    slots: UnsafeCell<Vec<Slot>>,
    capacity: usize,
    /// 写指针 —— 仅接收线程写入（Release）
    head: AtomicUsize,
    /// 读指针 —— 主线程推进；满时接收线程 CAS 推进（丢弃最旧）
    tail: AtomicUsize,
    /// 累计覆盖丢弃的采样点数（消费端跟不上时非零）
    overwritten: AtomicU64,
}

impl RingBuffer {
    /// 创建容量为 `capacity` 的环形缓冲区
    pub fn new(capacity: usize) -> Self {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(Slot {
                seq: AtomicU64::new(0),
                data: UnsafeCell::new(Sample::default()),
            });
        }
        Self {
            slots: UnsafeCell::new(slots),
            capacity,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            overwritten: AtomicU64::new(0),
        }
    }

    /// 写入一个采样点（接收线程调用）。永不阻塞。
    ///
    /// 缓冲区满时覆盖最旧数据（保留最新），overwritten 计数 +1。
    /// 覆盖操作与消费者读取并发时由槽位顺序锁保证一致性。
    pub fn push(&self, sample: Sample) {
        let h = self.head.load(Ordering::Relaxed);
        let t = self.tail.load(Ordering::Acquire);
        let in_use = h.wrapping_sub(t);
        if in_use >= self.capacity {
            // 满：CAS 推进 tail 丢弃最旧。仅当消费者尚未读取该槽
            // （tail 仍等于本线程观察值）时成功；失败说明消费者已
            // 推进腾出空间，无需丢弃。
            if self
                .tail
                .compare_exchange(t, t.wrapping_add(1), Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                self.overwritten.fetch_add(1, Ordering::Relaxed);
            }
        }

        let idx = h % self.capacity;
        // SAFETY: 槽位仅由 head/tail 与顺序锁协调访问
        unsafe {
            let slots = &*self.slots.get();
            let slot = &slots[idx];
            // 顺序锁：置奇（写入中）→ 写数据 → 置偶（完成，Release 发布）
            slot.seq.fetch_add(1, Ordering::Relaxed);
            *slot.data.get() = sample;
            slot.seq.fetch_add(1, Ordering::Release);
        }
        self.head.store(h.wrapping_add(1), Ordering::Release);
    }

    /// 批量读取采样点（主线程调用）
    ///
    /// 返回实际读取的数量（≤ buf.len()）。读取后 tail 指针前进。
    /// 覆盖语义下读到的始终是各槽位的最新数据。
    pub fn pop_batch(&self, buf: &mut [Sample]) -> usize {
        let t = self.tail.load(Ordering::Relaxed);
        let h = self.head.load(Ordering::Acquire);
        let available = h.wrapping_sub(t).min(self.capacity);
        let count = available.min(buf.len());

        for (i, out) in buf.iter_mut().enumerate().take(count) {
            let idx = (t.wrapping_add(i)) % self.capacity;
            // SAFETY: 顺序锁保证读到一致快照（写入并发时重试）
            unsafe {
                let slots = &*self.slots.get();
                let slot = &slots[idx];
                let sample = loop {
                    let s1 = slot.seq.load(Ordering::Acquire);
                    if s1 & 1 == 1 {
                        continue; // 生产者写入中，等待完成
                    }
                    let data = *slot.data.get();
                    let s2 = slot.seq.load(Ordering::Acquire);
                    if s1 == s2 {
                        break data; // 两次序号一致且为偶：快照稳定
                    }
                };
                *out = sample;
            }
        }

        self.tail.store(t.wrapping_add(count), Ordering::Release);
        count
    }

    /// 查询当前缓冲区中的可用采样点数
    pub fn available(&self) -> usize {
        let t = self.tail.load(Ordering::Relaxed);
        let h = self.head.load(Ordering::Acquire);
        h.wrapping_sub(t).min(self.capacity)
    }

    /// 查询总写入数（覆盖语义下 = 累计 push 次数）
    pub fn total_written(&self) -> usize {
        self.head.load(Ordering::Relaxed)
    }

    /// 累计覆盖丢弃的采样点数（消费端跟不上时非零，UI 提示用）
    pub fn overwritten(&self) -> u64 {
        self.overwritten.load(Ordering::Relaxed)
    }
}

// 环形缓冲区需要在线程间共享
// SAFETY: SPSC 模型 + 原子变量 + 槽位顺序锁 = 线程安全
unsafe impl Send for RingBuffer {}
unsafe impl Sync for RingBuffer {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_limits_retention() {
        // 容量参数生效：写入超过容量时覆盖最旧数据，保留最新 capacity 个点
        let ring = RingBuffer::new(100);
        for i in 0..150u64 {
            ring.push(Sample::new(i, i as f64 * 0.001, &[u64::from(1.0f32.to_bits())]));
        }
        assert_eq!(ring.available(), 100);
        assert_eq!(ring.overwritten(), 50);
        assert_eq!(ring.total_written(), 150);
        // 消费后读取到的是最新的 100 个点（seq 50..150）
        let mut buf = vec![Sample::default(); 200];
        let n = ring.pop_batch(&mut buf);
        assert_eq!(n, 100);
        assert_eq!(buf[0].seq, 50);
        assert_eq!(buf[99].seq, 149);
    }
}
