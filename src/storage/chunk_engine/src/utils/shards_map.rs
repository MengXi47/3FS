//! 把一個大 `HashMap` 拆成 `S` 個小 `HashMap` 的分片容器。
//!
//! **不是並發容器**：所有修改都要 `&mut self`，本身沒有任何鎖。在 crate 內它被放在
//! `ChunkAllocator` 裡 (`active_groups`、`frozen_groups`、`position_rc`)，由
//! `alloc/allocator.rs` 的 `Mutex<ChunkAllocator>` 統一保護。
//!
//! 分片的效果是每個分片各自擴容：單次 insert 觸發 rehash 時，只會搬動約 1/S 的元素，
//! 在持有 allocator 鎖的臨界區內不會出現整張大表一起 rehash 的長停頓
//! (`position_rc` 以 `1 << 20` 的容量建立，規模可觀)。
//!
//! ```text
//!   key --DefaultHasher--> h --(h % S)--> shards[i]: HashMap<K, V>
//!
//!   +----------+----------+-----+------------+
//!   | shard 0  | shard 1  | ... | shard S-1  |    S = 64 by default
//!   | HashMap  | HashMap  |     | HashMap    |
//!   +----------+----------+-----+------------+
//!
//!   iter(): shard 0 entries -> shard 1 entries -> ... (hash order, not sorted)
//! ```

use std::borrow::Borrow;
use std::collections::{
    hash_map::{DefaultHasher, Entry},
    HashMap,
};
use std::hash::{Hash, Hasher};

/// `S` 個 `HashMap` 組成的分片 map，`S` 預設 64。
pub struct ShardsMap<K, V, const S: usize = 64> {
    shards: [HashMap<K, V>; S],
}

/// 依序走過每個分片的迭代器；`inner_it` 是目前分片，`array_it` 是尚未走到的分片。
pub struct ShardsMapIter<'a, K, V> {
    array_it: std::slice::Iter<'a, HashMap<K, V>>,
    inner_it: std::collections::hash_map::Iter<'a, K, V>,
}

impl<K, V, const S: usize> ShardsMap<K, V, S>
where
    K: Eq + Hash,
{
    pub fn new() -> Self {
        Self {
            shards: [(); S].map(|_| Default::default()),
        }
    }

    /// 總容量平均分給各分片，每片取 `capacity / S` 向上到 2 的冪。
    pub fn with_capacity(capacity: usize) -> Self {
        let cap = (capacity / S).next_power_of_two();
        Self {
            shards: [(); S].map(|_| HashMap::with_capacity(cap)),
        }
    }

    /// 選分片。`DefaultHasher::new()` 用固定種子，同一個 key 永遠落在同一片；
    /// 分片內的 `HashMap` 另外用自己的隨機種子再雜湊一次。
    fn shard<Q>(key: &Q) -> usize
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let mut s = DefaultHasher::new();
        key.hash(&mut s);
        s.finish() as usize % S
    }

    pub fn get<Q>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.shards[Self::shard(k)].get(k)
    }

    pub fn get_mut<Q>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.shards[Self::shard(k)].get_mut(k)
    }

    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|m| m.is_empty())
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|m| m.len()).sum()
    }

    /// 依分片順序迭代，順序由雜湊決定。allocator 常用 `iter().next()` 取「任意一個」元素；
    /// 前面的分片都空時要逐片跳過，最差掃 S 個分片。
    pub fn iter(&self) -> ShardsMapIter<'_, K, V> {
        ShardsMapIter {
            array_it: self.shards[1..].iter(),
            inner_it: self.shards[0].iter(),
        }
    }

    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        self.shards[Self::shard(&k)].insert(k, v)
    }

    pub fn entry(&mut self, k: K) -> Entry<'_, K, V> {
        self.shards[Self::shard(&k)].entry(k)
    }

    pub fn remove<Q>(&mut self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.shards[Self::shard(k)].remove(k)
    }
}

impl<K, V, const S: usize> Default for ShardsMap<K, V, S>
where
    K: Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, K, V> Iterator for ShardsMapIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        // 目前分片走完就換下一片，直到所有分片都走完
        loop {
            if let Some(value) = self.inner_it.next() {
                return Some(value);
            } else if let Some(map) = self.array_it.next() {
                self.inner_it = map.iter();
            } else {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 驗證 insert / get / iter / entry / remove 在 4 個分片下的行為與一般 HashMap 一致
    #[test]
    fn test_shards_map() {
        let mut map = ShardsMap::<usize, usize, 4>::with_capacity(1024);
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);

        const N: usize = 1024;
        for i in 0..N {
            assert!(map.get(&i).is_none());
            map.insert(i, i * i);
        }
        assert!(!map.is_empty());
        assert_eq!(map.len(), N);

        assert_eq!(
            map.iter()
                .map(|(k, v)| {
                    assert_eq!(k * k, *v);
                })
                .count(),
            N
        );

        for i in 0..N {
            let value = map.get_mut(&i).unwrap();
            assert_eq!(i * i, *value);
            map.entry(i).and_modify(|v| *v += 1);
            assert_eq!(map.remove(&i).unwrap(), i * i + 1);
        }

        assert!(ShardsMap::<usize, usize, 4>::default().is_empty());
    }
}
