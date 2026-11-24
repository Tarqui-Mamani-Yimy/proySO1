//! Content-keyed cache of shaped text buffers for the renderer.
//!
//! Shaping a cosmic-text `Buffer` is the most expensive part of a draw, so the
//! renderer shapes each distinct content once and lets every text area that
//! shows the same content (any cell, any pane) point at the same buffer. The
//! cache is generic over the stored value so its rules can be unit-tested
//! without a font system.
//!
//! Rules:
//! - A value is keyed by everything that affects its shaping and drawing
//!   ([`GlyphKey`] for grid cells, [`TextKey`] for chrome and card text).
//! - All values are dropped when the font metrics change ([`FontMetricsKey`]),
//!   because a shaped buffer keeps the metrics it was built with.
//! - After a frame, when more than `capacity` values are held, the least
//!   recently used ones are dropped down to three quarters of `capacity`;
//!   values used in the frame that just ended are kept, so memory is bounded by
//!   `capacity` plus what one frame shows.

use std::collections::HashMap;
use std::hash::Hash;

/// Cache bound for grid cell glyphs (distinct character/style combinations).
pub const GLYPH_CAPACITY: usize = 4096;
/// Cache bound for chrome, help and card meta text runs.
pub const TEXT_CAPACITY: usize = 256;

/// Everything that changes how one grid cell's glyph is shaped and drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub character: char,
    pub foreground: [u8; 3],
    pub bold: bool,
    /// Width of the cell in columns (2 for wide characters); sets the buffer width.
    pub width_cells: usize,
}

/// Everything that changes how one chrome or card text run is shaped and drawn.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TextKey {
    pub text: String,
    pub color: [u8; 3],
    /// Buffer bounds as `f32::to_bits`, so equal floats compare equal.
    pub width_bits: u32,
    pub height_bits: u32,
}

impl TextKey {
    pub fn new(text: &str, color: [u8; 3], width: f32, height: f32) -> Self {
        Self {
            text: text.to_owned(),
            color,
            width_bits: width.to_bits(),
            height_bits: height.to_bits(),
        }
    }
}

/// Font metrics a shaped buffer depends on: font size and cell size in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FontMetricsKey([u32; 3]);

impl FontMetricsKey {
    pub fn new(font_size: f32, cell_width: f32, cell_height: f32) -> Self {
        Self([
            font_size.to_bits(),
            cell_width.to_bits(),
            cell_height.to_bits(),
        ])
    }
}

struct Entry<V> {
    value: V,
    last_used: u64,
}

/// Shaped values keyed by content, bounded by a least-recently-used sweep.
pub struct ShapedBufferCache<K, V> {
    entries: HashMap<K, Entry<V>>,
    metrics: Option<FontMetricsKey>,
    frame: u64,
    capacity: usize,
}

impl<K: Eq + Hash + Clone, V> ShapedBufferCache<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            metrics: None,
            frame: 0,
            capacity,
        }
    }

    /// Starts a frame: drops everything when the metrics changed, otherwise
    /// trims the cache back under its bound. Values looked up during the
    /// frame stay until the next `begin_frame`.
    pub fn begin_frame(&mut self, metrics: FontMetricsKey) {
        if self.metrics != Some(metrics) {
            self.entries.clear();
            self.metrics = Some(metrics);
        } else if self.entries.len() > self.capacity {
            self.evict();
        }
        self.frame += 1;
    }

    fn evict(&mut self) {
        let target = self.capacity * 3 / 4;
        let mut ages: Vec<(u64, K)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.last_used < self.frame)
            .map(|(key, entry)| (entry.last_used, key.clone()))
            .collect();
        ages.sort_unstable_by_key(|(last_used, _)| *last_used);
        let excess = self.entries.len().saturating_sub(target);
        for (_, key) in ages.into_iter().take(excess) {
            self.entries.remove(&key);
        }
    }

    /// Makes sure `key` has a value, building it with `make` only when absent,
    /// and marks it used in this frame.
    pub fn ensure(&mut self, key: &K, make: impl FnOnce() -> V) {
        let frame = self.frame;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_used = frame;
            return;
        }
        self.entries.insert(
            key.clone(),
            Entry {
                value: make(),
                last_used: frame,
            },
        );
    }

    /// The value for `key`, if it was ensured and not dropped since.
    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|entry| &entry.value)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> FontMetricsKey {
        FontMetricsKey::new(16.0, 9.0, 20.0)
    }

    fn glyph(character: char) -> GlyphKey {
        GlyphKey {
            character,
            foreground: [220, 220, 220],
            bold: false,
            width_cells: 1,
        }
    }

    #[test]
    fn equal_content_at_any_position_is_shaped_once() {
        let mut cache = ShapedBufferCache::new(GLYPH_CAPACITY);
        let mut shaped = 0;
        cache.begin_frame(metrics());
        for _position in 0..100 {
            cache.ensure(&glyph('a'), || {
                shaped += 1;
                'a'
            });
        }
        cache.begin_frame(metrics());
        cache.ensure(&glyph('a'), || {
            shaped += 1;
            'a'
        });
        assert_eq!(shaped, 1);
        assert_eq!(cache.get(&glyph('a')), Some(&'a'));
    }

    #[test]
    fn colour_weight_and_width_are_part_of_the_key() {
        let base = GlyphKey {
            character: '日',
            foreground: [255, 0, 0],
            bold: true,
            width_cells: 2,
        };
        let mut cache = ShapedBufferCache::new(GLYPH_CAPACITY);
        cache.begin_frame(metrics());
        let variants = [
            base,
            GlyphKey {
                foreground: [0, 255, 0],
                ..base
            },
            GlyphKey {
                bold: false,
                ..base
            },
            GlyphKey {
                width_cells: 1,
                ..base
            },
        ];
        for (index, key) in variants.iter().enumerate() {
            cache.ensure(key, || index);
        }
        assert_eq!(cache.len(), 4);
        for (index, key) in variants.iter().enumerate() {
            assert_eq!(cache.get(key), Some(&index));
        }
    }

    #[test]
    fn text_keys_compare_text_colour_and_bounds() {
        let key = TextKey::new("1.2s ", [110, 115, 125], 54.0, 20.0);
        assert_eq!(key, TextKey::new("1.2s ", [110, 115, 125], 54.0, 20.0));
        assert_ne!(key, TextKey::new("1.3s ", [110, 115, 125], 54.0, 20.0));
        assert_ne!(key, TextKey::new("1.2s ", [0, 0, 0], 54.0, 20.0));
        assert_ne!(key, TextKey::new("1.2s ", [110, 115, 125], 63.0, 20.0));
        assert_ne!(key, TextKey::new("1.2s ", [110, 115, 125], 54.0, 21.0));
    }

    #[test]
    fn metric_change_drops_every_value() {
        let mut cache = ShapedBufferCache::new(GLYPH_CAPACITY);
        cache.begin_frame(metrics());
        cache.ensure(&glyph('a'), || 1);
        cache.begin_frame(FontMetricsKey::new(16.0, 9.0, 20.0));
        assert_eq!(cache.get(&glyph('a')), Some(&1));
        cache.begin_frame(FontMetricsKey::new(32.0, 18.0, 40.0));
        assert_eq!(cache.len(), 0);
        let mut reshaped = false;
        cache.ensure(&glyph('a'), || {
            reshaped = true;
            2
        });
        assert!(reshaped);
    }

    #[test]
    fn eviction_bounds_the_cache_and_keeps_values_of_the_last_frame() {
        let capacity = 8;
        let mut cache = ShapedBufferCache::new(capacity);
        let characters: Vec<char> = ('a'..='z').collect();
        // Each frame shows four new characters, so old ones must be dropped.
        for frame in characters.chunks(4) {
            cache.begin_frame(metrics());
            for &character in frame {
                cache.ensure(&glyph(character), || character);
            }
            assert!(cache.len() <= capacity + frame.len());
        }
        let last = characters.chunks(4).last().unwrap();
        cache.begin_frame(metrics());
        assert!(cache.len() <= capacity);
        for &character in last {
            assert_eq!(cache.get(&glyph(character)), Some(&character));
        }
        assert_eq!(cache.get(&glyph('a')), None);
    }

    #[test]
    fn one_frame_larger_than_the_capacity_keeps_every_visible_value() {
        let mut cache = ShapedBufferCache::new(2);
        cache.begin_frame(metrics());
        for character in ['a', 'b', 'c', 'd'] {
            cache.ensure(&glyph(character), || character);
        }
        cache.begin_frame(metrics());
        // Nothing was used before the last frame, so nothing is dropped yet.
        assert_eq!(cache.len(), 4);
    }
}
