//! # viewport_culler
//! Fenwick Tree (Binary Indexed Tree) + Binary Lifting algorithm for $O(\log N)$
//! viewport visibility calculation ("only render what the user can actually see").

/// Viewport slice calculation result.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewportSlice {
    /// Start index (inclusive) in the flattened item list.
    pub start_idx: usize,
    /// End index (exclusive) in the flattened item list.
    pub end_idx: usize,
    /// Total height of all items above `start_idx` (px).
    pub top_spacer_height: f32,
    /// Total height of all items at or below `end_idx` (px).
    pub bottom_spacer_height: f32,
    /// Total height of all items combined (px).
    pub total_height: f32,
}

/// Fenwick Tree (1-indexed internally) storing item heights in integer quarter-pixels
/// to avoid floating-point drift while preserving sub-pixel precision.
#[derive(Debug, Clone, Default)]
pub struct FenwickViewportIndex {
    /// Individual item heights in quarter-pixels (1 px = 4 units).
    heights: Vec<i64>,
    /// Binary Indexed Tree array (1-based, length = heights.len() + 1).
    tree: Vec<i64>,
    /// Total sum of all heights in quarter-pixels.
    total_units: i64,
}

impl FenwickViewportIndex {
    const SCALE: f32 = 4.0;

    pub fn new() -> Self {
        Self {
            heights: Vec::new(),
            tree: vec![0],
            total_units: 0,
        }
    }

    #[inline]
    fn to_units(px: f32) -> i64 {
        (px.max(0.0) * Self::SCALE).round() as i64
    }

    #[inline]
    fn to_px(units: i64) -> f32 {
        (units.max(0) as f32) / Self::SCALE
    }

    /// Number of items currently indexed.
    #[inline]
    pub fn len(&self) -> usize {
        self.heights.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.heights.is_empty()
    }

    /// Clear all items.
    pub fn clear(&mut self) {
        self.heights.clear();
        self.tree.clear();
        self.tree.push(0);
        self.total_units = 0;
    }

    /// Rebuild or synchronize the Fenwick tree in $O(N)$ time from a slice of heights,
    /// or incrementally update changed entries in $O(\log N)$ time.
    pub fn sync_heights(&mut self, new_heights_px: &[f32]) {
        let new_len = new_heights_px.len();
        if new_len < self.heights.len() {
            self.rebuild(new_heights_px);
            return;
        }

        // Update existing overlapping items if their height changed
        let common = self.heights.len();
        for (i, &h_px) in new_heights_px.iter().enumerate().take(common) {
            let u = Self::to_units(h_px);
            let delta = u - self.heights[i];
            if delta != 0 {
                self.heights[i] = u;
                self.total_units += delta;
                self.add_internal(i + 1, delta);
            }
        }

        // Append newly added items
        for &h_px in &new_heights_px[common..] {
            self.push_height(h_px);
        }
    }

    /// Rebuild the entire Fenwick tree in strictly $O(N)$ time.
    pub fn rebuild(&mut self, heights_px: &[f32]) {
        let n = heights_px.len();
        self.heights.clear();
        self.heights.reserve(n);
        self.tree.clear();
        self.tree.resize(n + 1, 0);
        self.total_units = 0;

        for (i, &h_px) in heights_px.iter().enumerate() {
            let u = Self::to_units(h_px);
            self.heights.push(u);
            self.total_units += u;
            let idx = i + 1;
            self.tree[idx] += u;
            let parent = idx + idx.isolate_lowest_one();
            if parent <= n {
                self.tree[parent] += self.tree[idx];
            }
        }
    }

    /// Append a single item height in $O(\log N)$ time.
    pub fn push_height(&mut self, height_px: f32) {
        let u = Self::to_units(height_px);
        let prev_len = self.heights.len();
        self.heights.push(u);
        self.total_units += u;
        let idx = self.heights.len();
        let lowbit = idx.isolate_lowest_one();
        let range_sum = self.prefix_units(prev_len) - self.prefix_units(idx - lowbit);
        self.tree.push(u + range_sum);
    }

    /// Update the height of item `index` in $O(\log N)$ time.
    pub fn update_height(&mut self, index: usize, new_height_px: f32) {
        if index >= self.heights.len() {
            return;
        }
        let u = Self::to_units(new_height_px);
        let delta = u - self.heights[index];
        if delta == 0 {
            return;
        }
        self.heights[index] = u;
        self.total_units += delta;
        self.add_internal(index + 1, delta);
    }

    #[inline]
    fn add_internal(&mut self, mut idx_1based: usize, delta: i64) {
        let n = self.heights.len();
        while idx_1based <= n {
            self.tree[idx_1based] += delta;
            idx_1based += idx_1based.isolate_lowest_one();
        }
    }

    /// Prefix sum of heights for items `[0..count)` in quarter-pixels in $O(\log N)$ time.
    #[inline]
    fn prefix_units(&self, count: usize) -> i64 {
        let mut idx = count.min(self.heights.len());
        let mut sum = 0;
        while idx > 0 {
            sum += self.tree[idx];
            idx &= idx - 1;
        }
        sum
    }

    /// Query the Y-offset (top coordinate) of item `index` in pixels in $O(\log N)$ time.
    #[inline]
    pub fn offset_of(&self, index: usize) -> f32 {
        Self::to_px(self.prefix_units(index))
    }

    /// Total height of all items in pixels.
    #[inline]
    pub fn total_height(&self) -> f32 {
        Self::to_px(self.total_units)
    }

    /// Return the current height of item `index` in pixels in $O(1)$ time.
    #[inline]
    pub fn get_height(&self, index: usize) -> f32 {
        self.heights.get(index).copied().map(Self::to_px).unwrap_or(0.0)
    }

    /// Binary lifting on the Fenwick tree in $O(\log N)$ time.
    /// Returns the smallest 0-based item index `i` such that `prefix_units(i + 1) > target_units`.
    /// If `target_units >= total_units`, returns `self.heights.len()`.
    fn lower_bound_item(&self, target_units: i64) -> usize {
        let n = self.heights.len();
        if n == 0 || target_units <= 0 {
            return 0;
        }
        if target_units >= self.total_units {
            return n;
        }

        let mut idx = 0usize;
        let mut current_sum = 0i64;
        // Highest power of two <= n
        let mut bit = 1usize << (usize::BITS - 1 - n.leading_zeros());

        while bit > 0 {
            let next = idx + bit;
            if next <= n && current_sum + self.tree[next] <= target_units {
                idx = next;
                current_sum += self.tree[next];
            }
            bit >>= 1;
        }
        idx
    }

    /// Compute the exact visible slice `[start_idx, end_idx)` for the given viewport
    /// in $O(\log N)$ time using Fenwick tree binary lifting.
    ///
    /// - `scroll_y`: positive scroll offset from top (`-Flickable.viewport-y`).
    /// - `viewport_height`: height of the visible scroll container.
    /// - `overscan_px`: extra buffer above and below the viewport to prevent flicker.
    pub fn query_viewport(
        &self,
        scroll_y: f32,
        viewport_height: f32,
        overscan_px: f32,
    ) -> ViewportSlice {
        let n = self.heights.len();
        if n == 0 {
            return ViewportSlice {
                start_idx: 0,
                end_idx: 0,
                top_spacer_height: 0.0,
                bottom_spacer_height: 0.0,
                total_height: 0.0,
            };
        }

        let y_min = (scroll_y.max(0.0) - overscan_px.max(0.0)).max(0.0);
        let y_max = scroll_y.max(0.0) + viewport_height.max(1.0) + overscan_px.max(0.0);

        let min_units = Self::to_units(y_min);
        let max_units = Self::to_units(y_max);

        let start_idx = self.lower_bound_item(min_units).min(n.saturating_sub(1));
        let mut end_idx = self.lower_bound_item(max_units);
        if end_idx < n {
            // Include the item that straddles `y_max`
            end_idx += 1;
        }
        end_idx = end_idx.clamp(start_idx + 1, n);

        let top_units = self.prefix_units(start_idx);
        let end_units = self.prefix_units(end_idx);
        let bottom_units = (self.total_units - end_units).max(0);

        ViewportSlice {
            start_idx,
            end_idx,
            top_spacer_height: Self::to_px(top_units),
            bottom_spacer_height: Self::to_px(bottom_units),
            total_height: Self::to_px(self.total_units),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fenwick_culling_10000_blocks() {
        let mut index = FenwickViewportIndex::new();
        // 10,000 blocks, each 50px tall -> total 500,000px
        let heights = vec![50.0f32; 10_000];
        index.sync_heights(&heights);

        assert_eq!(index.total_height(), 500_000.0);

        // Scroll to Y = 250,000px (block 5000), viewport = 600px, overscan = 100px
        // Visible Y range: [249,900 .. 250,700]
        // Block 4998 spans [249,900 .. 249,950]
        // Block 5013 spans [250,650 .. 250,700]
        let slice = index.query_viewport(250_000.0, 600.0, 100.0);
        assert_eq!(slice.start_idx, 4998);
        assert_eq!(slice.end_idx, 5015);
        assert_eq!(slice.top_spacer_height, 4998.0 * 50.0);
        assert_eq!(
            slice.top_spacer_height
                + ((slice.end_idx - slice.start_idx) as f32 * 50.0)
                + slice.bottom_spacer_height,
            500_000.0
        );
    }

    #[test]
    fn test_dynamic_height_update_in_log_n() {
        let mut index = FenwickViewportIndex::new();
        index.sync_heights(&[40.0, 60.0, 80.0, 100.0]);
        assert_eq!(index.offset_of(2), 100.0);

        // Block 1 expands from 60px to 160px (+100px)
        index.update_height(1, 160.0);
        assert_eq!(index.offset_of(2), 200.0);
        assert_eq!(index.total_height(), 380.0);
    }
}
