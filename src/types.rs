#![allow(unused)]
pub(crate) type BorgResult<T> = anyhow::Result<T>;

use std::collections::VecDeque;

#[derive(Debug, Default)]
pub(crate) struct RingBuffer<T> {
    deque: VecDeque<T>,
    capacity: usize,
}

// TODO: Use const generics somehow
impl<T> RingBuffer<T> {
    pub(crate) fn new(size: usize) -> Self {
        Self {
            deque: VecDeque::with_capacity(size),
            capacity: size,
        }
    }

    pub(crate) fn push_back(&mut self, item: T) {
        self.deque.push_back(item);
        if self.deque.len() > self.capacity {
            self.deque.pop_front();
        }
    }

    pub(crate) fn front(&self) -> Option<&T> {
        self.deque.front()
    }

    pub(crate) fn back(&self) -> Option<&T> {
        self.deque.back()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.deque.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.deque.iter()
    }
}

impl<T> FromIterator<T> for RingBuffer<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut r = RingBuffer::new(256);
        for item in iter.into_iter() {
            r.push_back(item)
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::RingBuffer;

    #[test]
    fn test_pushes() {
        let mut r = RingBuffer::new(3);
        for c in 'A'..='C' {
            r.push_back(c);
        }
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec!['A', 'B', 'C']);
        assert_eq!(r.front(), Some(&'A'));
        assert_eq!(r.back(), Some(&'C'));
        r.push_back('D');
        assert_eq!(r.front(), Some(&'B'));
        assert_eq!(r.back(), Some(&'D'));
        r.push_back('E');
        assert_eq!(r.back(), Some(&'E'));
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec!['C', 'D', 'E']);
        r.push_back('F');
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec!['D', 'E', 'F']);
        r.push_back('G');
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec!['E', 'F', 'G']);
    }

    #[test]
    fn test_empty_iter() {
        let empty: RingBuffer<u32> = RingBuffer::new(256);
        let test: Vec<u32> = Vec::new();
        assert_eq!(empty.iter().copied().collect::<Vec<_>>(), test);
    }

    #[test]
    fn test_larger() {
        let big: RingBuffer<u32> = (0..=1024).collect();
        assert_eq!(
            big.iter().copied().collect::<Vec<_>>(),
            (769..=1024).collect::<Vec<_>>()
        );
    }
}
