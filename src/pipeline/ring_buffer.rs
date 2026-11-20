use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use super::sample::Sample;

/// 单槽位：数据 + 逻辑位置戳 + 顺序锁发布序号
///
/// `seq` 偶数 = 数据稳定可读；奇数 = 生产者写入中。
/// 生产者写入：seq 置奇 → 写数据/pos → seq 置偶（Release 发布）；
/// 消费者读取：读 seq（Acquire）→ 读数据 → 再读 seq，两次一致且为偶
/// 才接受，否则重试——保证不会读到撕裂数据。
///
/// `pos` 记录本槽最后一次写入对应的**全局逻辑位置**（第几次 push）。
/// 满载覆盖与消费者读取并发时，消费者期望位置 p 的槽可能已被生产者
/// 套圈写成更新的样本（位置 p+capacity·k），用 pos 戳即可识别并重新
/// 对齐，避免重复/乱序样本进入波形与 CSV 导出。
struct Slot {
    seq: AtomicU64,
    pos: AtomicU64,
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
                pos: AtomicU64::new(0),
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
            // 顺序锁：置奇（写入中）→ 写数据/位置戳 → 置偶（完成，Release 发布）
            slot.seq.fetch_add(1, Ordering::Relaxed);
            *slot.data.get() = sample;
            slot.pos.store(h as u64, Ordering::Relaxed);
            slot.seq.fetch_add(1, Ordering::Release);
        }
        self.head.store(h.wrapping_add(1), Ordering::Release);
    }

    /// 批量读取采样点（主线程调用）
    ///
    /// 返回实际读取的数量（≤ buf.len()）。读取后 tail 指针前进。
    /// 覆盖语义下读到的始终是各槽位的最新数据。
    ///
    /// 读取期间生产者可能满载套圈（覆盖消费者尚未读到的槽位）：每个槽
    /// 带有位置戳，读到「位置戳 ≠ 期望位置」说明该位置的样本已被覆盖
    /// 丢弃，此时以当前丢弃边界（tail）重新对齐再继续读，保证返回的
    /// 样本**不重复、不乱序**（允许因覆盖产生缺口）。
    pub fn pop_batch(&self, buf: &mut [Sample]) -> usize {
        let mut head = self.head.load(Ordering::Acquire);
        let mut pos = self.tail.load(Ordering::Relaxed);
        let mut n = 0usize;

        while n < buf.len() {
            // head - pos 在模意义下超出 capacity 说明 pos 已越过 head
            // （重对齐后 head 快照过期，或 push 的 discard CAS 与 head
            // 回写之间的瞬时窗口）：无可读，等待下一批
            let ahead = head.wrapping_sub(pos);
            let avail = if ahead > self.capacity { 0 } else { ahead };
            if avail == 0 {
                break;
            }
            let idx = pos % self.capacity;
            // SAFETY: 顺序锁保证读到一致快照（写入并发时重试）
            let (sample, stamped) = unsafe {
                let slots = &*self.slots.get();
                let slot = &slots[idx];
                loop {
                    let s1 = slot.seq.load(Ordering::Acquire);
                    if s1 & 1 == 1 {
                        continue; // 生产者写入中，等待完成
                    }
                    let data = *slot.data.get();
                    let stamped = slot.pos.load(Ordering::Acquire);
                    let s2 = slot.seq.load(Ordering::Acquire);
                    if s1 == s2 {
                        break (data, stamped); // 两次序号一致且为偶：快照稳定
                    }
                }
            };
            if stamped != pos as u64 {
                // 套圈：期望位置 p 的槽已被覆盖为更晚的样本（p + capacity·k）。
                // 该位置的原始样本已不可恢复，重新对齐到当前丢弃边界。
                // head 必须同步刷新——否则过期的 head 快照会把「尚未生产
                // 的位置」也当作可读，读到旧样本后反复失配空转（压力测试
                // 抓到的实现缺陷）。
                pos = self.tail.load(Ordering::Acquire);
                head = self.head.load(Ordering::Acquire);
                continue;
            }
            buf[n] = sample;
            n += 1;
            pos = pos.wrapping_add(1);
        }

        self.advance_tail(pos);
        n
    }

    /// 推进 tail 且不回退。
    ///
    /// 本写入与生产者满载丢弃的 CAS 推进并发：若生产者已把 tail 推得
    /// 比本次读取终点更远（消费端被套圈），无条件 store 会把 tail 倒
    /// 回去——已被覆盖丢弃的位置重新「可用」，后续批次会读到重复样本。
    /// CAS 循环取两者较大值（模 2^N 意义下的「前方」）。
    fn advance_tail(&self, new_tail: usize) {
        let mut cur = self.tail.load(Ordering::Relaxed);
        loop {
            let ahead = new_tail.wrapping_sub(cur);
            if ahead == 0 || ahead > usize::MAX / 2 {
                return; // cur 已在 new_tail 前方或相等：不回退
            }
            match self.tail.compare_exchange_weak(
                cur,
                new_tail,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
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
    use std::sync::Arc;
    use std::thread;

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

    #[test]
    fn tail_advance_never_regresses() {
        // 消费者基于过期快照的回写不得倒退：生产者满载 CAS 已把 tail
        // 推进到更前时，回写应保留更大值（否则被丢弃覆盖的位置「复活」，
        // 后续批次读到重复样本——原缺陷 ⑤ 的直接机制）
        let ring = RingBuffer::new(4);
        ring.tail.store(5, Ordering::Relaxed);
        ring.head.store(9, Ordering::Relaxed);
        ring.advance_tail(3); // 3 < 5：不回退
        assert_eq!(ring.tail.load(Ordering::Relaxed), 5);
        ring.advance_tail(7); // 7 > 5：正常推进
        assert_eq!(ring.tail.load(Ordering::Relaxed), 7);
        ring.advance_tail(7); // 相等：无操作
        assert_eq!(ring.tail.load(Ordering::Relaxed), 7);
    }

    /// 生产者持续满载套圈 + 消费者小批量慢速读取的并发压力测试：
    /// 消费端看到的样本必须严格递增（不重复、不乱序）。
    /// 这是原缺陷 ⑤ 的完整复现场景（导出对话框阻塞 UI 2 秒即触发）。
    #[test]
    fn lap_producer_never_yields_duplicates_or_disorder() {
        const TOTAL: u64 = 30_000;
        let ring = Arc::new(RingBuffer::new(8));
        let producer_ring = Arc::clone(&ring);
        let producer = thread::spawn(move || {
            for i in 0..TOTAL {
                producer_ring.push(Sample::new(i, 0.0, &[i]));
            }
        });

        let mut last_seq: Option<u64> = None;
        let mut consumed = 0u64;
        let mut buf = vec![Sample::default(); 3];
        let mut iters = 0usize;
        loop {
            let n = ring.pop_batch(&mut buf);
            for s in &buf[..n] {
                if let Some(prev) = last_seq {
                    assert!(
                        s.seq > prev,
                        "样本重复或乱序: prev={} got={} (consumed={})",
                        prev, s.seq, consumed
                    );
                }
                last_seq = Some(s.seq);
                consumed += 1;
            }
            iters += 1;
            if n == 0 {
                // 小睡片刻，让生产者有机会套圈（覆盖未读槽位）
                thread::sleep(std::time::Duration::from_micros(50));
            }
            if producer.is_finished() {
                if ring.available() == 0 {
                    break;
                }
                assert!(iters <= 5000, "生产结束后缓冲区应在数千次循环内排空");
            } else {
                assert!(iters <= 500_000, "压力测试超时");
            }
        }
        producer.join().unwrap();

        assert!(consumed <= TOTAL, "消费数 {} 不得超过生产数 {}", consumed, TOTAL);
        assert!(consumed > 0, "至少应消费到部分样本");
        assert_eq!(
            last_seq,
            Some(TOTAL - 1),
            "生产结束后应一直读到最新样本"
        );
    }

    #[test]
    fn consumer_fully_drains_after_lap() {
        // 单线程确定性场景：满载后生产者套圈多圈，消费端一次读出，
        // 结果必须是「最新 capacity 个」且严格递增
        let ring = RingBuffer::new(4);
        for i in 0..13u64 {
            ring.push(Sample::new(i, 0.0, &[i]));
        }
        let mut buf = vec![Sample::default(); 4];
        let n = ring.pop_batch(&mut buf);
        assert_eq!(n, 4);
        for (k, s) in buf[..n].iter().enumerate() {
            assert_eq!(s.seq, 9 + k as u64);
        }
        assert_eq!(ring.pop_batch(&mut buf), 0, "读空后不应再有样本");
    }
}
