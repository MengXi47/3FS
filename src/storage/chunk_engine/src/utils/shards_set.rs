//! `ShardsMap` 的 set 版本：`S` 個 `HashSet` 組成的分片集合，設計與限制同 `shards_map.rs`。
//!
//! 同樣**沒有鎖**，由外層 `Mutex<ChunkAllocator>` 保護。crate 內用來存放 group 集合：
//!
//! ```text
//!   ChunkAllocator.full_groups          : ShardsSet<GroupId>
//!   ChunkAllocator.active_levels[0..4]  : ShardsSet<GroupId>   (one set per fill level)
//!   GroupAllocator.allocated_groups     : ShardsSet<GroupId>   (pre-fallocated pool)
//!   GroupAllocator.unallocated_groups   : ShardsSet<GroupId>   (holes / freed ids)
//! ```
//!
//! 配置器取 group 時用 `iter().next()`，拿到的是雜湊順序上的第一個，並非最小的 GroupId。

use std::borrow::Borrow;
use std::collections::{hash_map::DefaultHasher, HashSet};
use std::hash::{Hash, Hasher};

/// `S` 個 `HashSet` 組成的分片集合，`S` 預設 64。
pub struct ShardsSet<T, const S: usize = 64> {
    shards: [HashSet<T>; S],
}

/// 依序走過每個分片的迭代器；`inner_it` 是目前分片，`array_it` 是尚未走到的分片。
pub struct ShardsSetIter<'a, T> {
    array_it: std::slice::Iter<'a, HashSet<T>>,
    inner_it: std::collections::hash_set::Iter<'a, T>,
}

impl<T, const S: usize> ShardsSet<T, S>
where
    T: Eq + Hash,
{
    pub fn new() -> Self {
        Self {
            shards: [(); S].map(|_| Default::default()),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let cap = (capacity / S).next_power_of_two();
        Self {
            shards: [(); S].map(|_| HashSet::with_capacity(cap)),
        }
    }

    /// 選分片，與 `ShardsMap::shard` 相同：固定種子的 `DefaultHasher` 取模 `S`。
    fn shard<Q>(key: &Q) -> usize
    where
        T: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let mut s = DefaultHasher::new();
        key.hash(&mut s);
        s.finish() as usize % S
    }

    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.shards[Self::shard(value)].contains(value)
    }

    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|m| m.is_empty())
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|m| m.len()).sum()
    }

    pub fn iter(&self) -> ShardsSetIter<'_, T> {
        ShardsSetIter {
            array_it: self.shards[1..].iter(),
            inner_it: self.shards[0].iter(),
        }
    }

    pub fn insert(&mut self, value: T) -> bool {
        self.shards[Self::shard(&value)].insert(value)
    }

    pub fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.shards[Self::shard(value)].remove(value)
    }
}

impl<T, const S: usize> Default for ShardsSet<T, S>
where
    T: Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, T> Iterator for ShardsSetIter<'a, T> {
    type Item = &'a T;

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

    // 驗證 insert / contains / remove 在 4 個分片下的行為 (函式名沿用 shards_map 的命名)
    #[test]
    fn test_shards_map() {
        let mut set = ShardsSet::<usize, 4>::with_capacity(1024);
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);

        const N: usize = 1024;
        for i in 0..N {
            assert!(!set.contains(&i));
            assert!(set.insert(i));
        }
        assert!(!set.is_empty());
        assert_eq!(set.len(), N);

        for i in 0..N {
            assert!(set.contains(&i));
            assert!(set.remove(&i));
            assert!(!set.remove(&i));
        }

        assert!(ShardsSet::<usize>::default().is_empty());
    }
}
