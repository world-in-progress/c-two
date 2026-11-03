//! Owner-local free bits, accessed under the buddy allocator's local mutex.

/// One level of the buddy tree: 1 is free, 0 is used or absent.
pub struct LevelBitmap {
    words: Vec<u64>,
    num_blocks: usize,
}

impl LevelBitmap {
    pub fn new(num_blocks: usize) -> Self {
        Self {
            words: vec![0; num_blocks.div_ceil(64)],
            num_blocks,
        }
    }

    pub fn alloc_one(&mut self) -> Option<usize> {
        for (index, word) in self.words.iter_mut().enumerate() {
            if *word != 0 {
                let bit = word.trailing_zeros() as usize;
                *word &= !(1 << bit);
                return Some(index * 64 + bit);
            }
        }
        None
    }

    pub fn free_one(&mut self, block: usize) {
        debug_assert!(block < self.num_blocks);
        self.words[block / 64] |= 1 << (block % 64);
    }

    pub fn is_free(&self, block: usize) -> bool {
        debug_assert!(block < self.num_blocks);
        self.words[block / 64] & (1 << (block % 64)) != 0
    }

    pub fn mark_used(&mut self, block: usize) {
        debug_assert!(block < self.num_blocks);
        self.words[block / 64] &= !(1 << (block % 64));
    }
}

pub fn num_levels(data_size: usize, min_block: usize) -> usize {
    (data_size / min_block).ilog2() as usize + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_words_and_reuse() {
        let mut bitmap = LevelBitmap::new(131);
        for block in 0..131 {
            bitmap.free_one(block);
        }
        for block in 0..131 {
            assert_eq!(bitmap.alloc_one(), Some(block));
        }
        assert_eq!(bitmap.alloc_one(), None);
        for block in [0, 63, 64, 130] {
            bitmap.free_one(block);
            assert!(bitmap.is_free(block));
        }
        bitmap.mark_used(64);
        for block in [0, 63, 130] {
            assert_eq!(bitmap.alloc_one(), Some(block));
        }
        assert_eq!(bitmap.alloc_one(), None);
    }
}
