#![allow(unused)]
pub(crate) type BorgResult<T> = anyhow::Result<T>;

pub(crate) struct RingBuffer<T> {
    data: Vec<T>,
    capacity: usize,
    head: usize,
    tail: usize,
}

// TODO: Use const generics somehow
impl<T> RingBuffer<T> {
    pub(crate) fn new(size: usize) -> Self {
        Self {
            data: Vec::with_capacity(size),
            capacity: size,
            head: 0,
            tail: 0,
        }
    }

    pub(crate) fn push_back(&mut self, item: T) {
        if self.data.is_empty() {
            self.data.push(item);
            return;
        }
        // We have space in the collection
        if self.data.len() < self.capacity {
            self.data.push(item);
            self.tail += 1;
        } else {
            self.data[self.head] = item;
            self.head = (self.head + 1) % self.capacity;
            self.tail = (self.tail + 1) % self.capacity;
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    pub(crate) fn iter(&self) -> RingBufferIter<'_, T> {
        RingBufferIter::new(self.data.as_slice(), self.head, self.tail)
    }
}

pub(crate) struct RingBufferIter<'a, T> {
    contents: &'a [T],
    head: usize,
    tail: usize,
    position: usize,
    // TODO: Figure out the real stopping condition lol
    done: bool,
}

impl<'a, T> RingBufferIter<'a, T> {
    pub(crate) fn new(contents: &'a [T], head: usize, tail: usize) -> Self {
        Self {
            contents,
            head,
            tail,
            position: head,
            done: false,
        }
    }
}

impl<'a, T> Iterator for RingBufferIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.contents.is_empty() {
            return None;
        }
        if self.position == self.tail {
            self.done = true;
        }
        let item = &self.contents[self.position];
        self.position = (self.position + 1) % self.contents.len();
        Some(item)
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
        assert_eq!(r.data, vec!['A', 'B', 'C']);
        assert_eq!(r.tail, 2);
        assert!(r.data.len() == r.data.capacity());
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec!['A', 'B', 'C']);
        r.push_back('D');
        assert_eq!(r.data, vec!['D', 'B', 'C']);
        assert_eq!(r.head, 1);
        assert_eq!(r.tail, 0);
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec!['B', 'C', 'D']);
        r.push_back('E');
        assert_eq!(r.data, vec!['D', 'E', 'C']);
        assert_eq!(r.head, 2);
        assert_eq!(r.tail, 1);
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec!['C', 'D', 'E']);
        r.push_back('F');
        assert_eq!(r.data, vec!['D', 'E', 'F']);
        assert_eq!(r.head, 0);
        assert_eq!(r.tail, 2);
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec!['D', 'E', 'F']);
        r.push_back('G');
        assert_eq!(r.data, vec!['G', 'E', 'F']);
        assert_eq!(r.head, 1);
        assert_eq!(r.tail, 0);
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
