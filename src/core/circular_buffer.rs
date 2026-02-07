use std::collections::VecDeque;
use std::ops::Index;

/// Fixed-capacity ring buffer.
///
/// This is a small wrapper around `VecDeque`:
/// - `push` is O(1) and evicts from the front when full.
/// - Memory usage is bounded by `capacity`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CircularBuffer<T> {
    buffer: VecDeque<T>,
    capacity: usize,
}

impl<T> CircularBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn front(&self) -> Option<&T> {
        self.buffer.front()
    }

    pub fn push(&mut self, item: T) {
        // Capacity==0 means "store nothing". Without this guard, VecDeque could grow unbounded.
        if self.capacity == 0 {
            return;
        }

        if self.buffer.len() == self.capacity {
            self.buffer.pop_front();
        }
        self.buffer.push_back(item);
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.buffer.iter().take(self.len())
    }

    pub fn iter_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut T> + '_ {
        self.buffer.iter_mut()
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.buffer.len() == self.capacity
    }

    pub fn back(&self) -> Option<&T> {
        self.buffer.back()
    }

    pub fn back_mut(&mut self) -> Option<&mut T> {
        self.buffer.back_mut()
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.buffer.get(index)
    }

    pub fn front_mut(&mut self) -> Option<&mut T> {
        self.buffer.front_mut()
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    pub fn pop_front(&mut self) -> Option<T> {
        self.buffer.pop_front()
    }

    pub fn pop_back(&mut self, index: usize) -> Option<T> {
        if index >= self.len() {
            return None;
        }

        let last_index = self.len() - 1;
        self.buffer.swap(index, last_index);
        self.buffer.pop_back()
    }

    pub fn remove(&mut self, item: &T) -> Option<T>
    where
        T: PartialEq,
    {
        if let Some(index) = self.buffer.iter().position(|x| x == item) {
            self.pop_back(index)
        } else {
            None
        }
    }

    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&T) -> bool,
    {
        let len = self.buffer.len();
        let mut del = 0;
        for i in 0..len {
            if !f(&self.buffer[i]) {
                del += 1;
            } else if del > 0 {
                self.buffer.swap(i - del, i);
            }
        }
        if del > 0 {
            self.buffer.truncate(len - del);
        }
    }

    pub fn last(&self, n: usize) -> Option<&[T]> {
        let len = self.buffer.len();
        if n == 0 || n > len {
            return None;
        }
        let (first, second) = self.buffer.as_slices();
        let start_idx = len - n;
        if second.is_empty() {
            Some(&first[start_idx..])
        } else if start_idx >= first.len() {
            Some(&second[start_idx - first.len()..])
        } else {
            None
        }
    }

    pub fn second_to_last(&self) -> Option<&T> {
        if self.len() >= 2 {
            self.get(self.len() - 2)
        } else {
            None
        }
    }
}

impl<T> Index<usize> for CircularBuffer<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        &self.buffer[index]
    }
}

impl<'a, T> IntoIterator for &'a CircularBuffer<T> {
    type Item = &'a T;
    type IntoIter = std::iter::Take<std::collections::vec_deque::Iter<'a, T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.buffer.iter().take(self.len())
    }
}

impl<T> IntoIterator for CircularBuffer<T> {
    type Item = T;
    type IntoIter = std::collections::vec_deque::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.buffer.into_iter()
    }
}
