//! Slab allocator layout — #150 phase 1.
//!
//! See docs/state/features/150_allocator_rewrite.md for design.
//!
//! Layout-only: constants + helpers.  No IR generation, no alloc logic.

/// Slab layout constants for the page-reclaiming allocator.
pub struct SlabLayout;

impl SlabLayout {
    // ---------- Slab geometry ----------

    /// Default slab size, in bytes (64 KiB).
    ///
    /// Picked to fit "many chunks" for small classes (class 0 = 16 B → ~4000
    /// chunks per slab) while keeping per-slab metadata cheap.  Classes whose
    /// `class_size` exceeds the room left after the header will return 0 from
    /// `chunks_per_slab(_)` — those need a per-class slab size (phase 2).
    pub const DEFAULT_SLAB_SIZE: i64 = 64 * 1024;

    /// Slabs must be aligned to `DEFAULT_SLAB_SIZE` (well, the per-class slab
    /// size — which is always a power-of-two multiple of `DEFAULT_SLAB_SIZE`).
    /// This is what lets free(chunk_addr) recover the owning slab via
    /// `chunk_addr & !(slab_size - 1)`.
    pub const SLAB_ALIGN: i64 = Self::DEFAULT_SLAB_SIZE;

    /// Maximum payload alignment we promise to chunks (matches malloc).
    pub const CHUNK_ALIGN: i32 = 16;

    // ---------- Slab header offsets ----------

    /// `class_id : i64` — which size class owns this slab.
    pub const HDR_CLASS_ID: i32 = 0;
    /// `free_count : i64` — number of currently-free chunks in this slab.
    pub const HDR_FREE_COUNT: i32 = 8;
    /// `next_slab : i64` — next slab in this class's linked list (or 0).
    pub const HDR_NEXT_SLAB: i32 = 16;
    /// `prev_slab : i64` — previous slab in this class's linked list (or 0).
    pub const HDR_PREV_SLAB: i32 = 24;
    /// Bitmap of free chunks starts here.  Bit set = chunk free.
    /// Length = ceil(chunks_per_slab / 8) bytes; padded so chunks start
    /// 16-byte aligned.
    pub const HDR_BITMAP_START: i32 = 32;
    /// Fixed (non-bitmap) part of the header, in bytes.
    pub const HDR_FIXED_BYTES: i32 = 32;

    // ---------- Per-class state table offsets ----------
    //
    // Replaces the current single-pointer `free_list_heads[21]` table
    // with a 24-byte struct per class:
    //
    //   { slabs_head : i64, current_slab : i64, _padding : i64 }
    //
    // 24 B × 21 = 504 B total.  Padding keeps each entry 16-aligned for
    // cheap indexing (`base + class_idx * 24`) without unaligned loads;
    // also reserves a slot for the future "chunks_per_slab cache" field
    // so we don't recompute it on every alloc.

    /// Offset of `slabs_head` within a class-state entry.
    pub const CLASS_SLABS_HEAD: i32 = 0;
    /// Offset of `current_slab` (fast-path "last allocated from") within a
    /// class-state entry.
    pub const CLASS_CURRENT_SLAB: i32 = 8;
    /// Offset of the reserved padding word (future: cached
    /// `chunks_per_slab`).
    pub const CLASS_PADDING: i32 = 16;
    /// Size of a single class-state entry, in bytes.
    pub const CLASS_STATE_SIZE: i32 = 24;

    /// Number of size classes.  Mirrors the constant in
    /// `funcs/alloc.rs` — keep these in sync (phase 2 will consolidate).
    pub const NUM_CLASSES: usize = 21;

    // ---------- Helpers ----------

    /// Size in bytes of class `class_idx`.  Mirrors the existing allocator:
    /// class 0 = 16 B, class 1 = 32 B, … class 20 = 16 MiB.
    pub const fn class_size(class_idx: usize) -> usize {
        1usize << (class_idx + 4)
    }

    /// How many chunks of class `class_idx` fit in a default-sized slab,
    /// after accounting for the header + bitmap + 16-byte payload alignment.
    ///
    /// Returns 0 if even one chunk does not fit — those classes need a
    /// per-class oversized slab (designed in phase 2).
    pub fn chunks_per_slab(class_idx: usize) -> usize {
        let chunk_size = Self::class_size(class_idx);
        let slab_size = Self::DEFAULT_SLAB_SIZE as usize;
        let header = Self::HDR_FIXED_BYTES as usize;
        let align = Self::CHUNK_ALIGN as usize;

        if slab_size <= header + chunk_size {
            return 0;
        }

        // Iterate to convergence: bitmap_bytes depends on chunks, chunks
        // depends on payload_start which depends on bitmap_bytes.  Three
        // rounds is always enough — guard with a hard cap for safety.
        let mut chunks = (slab_size - header) / chunk_size;
        for _ in 0..8 {
            let bitmap_bytes = (chunks + 7) / 8;
            let payload_start = (header + bitmap_bytes + align - 1) & !(align - 1);
            if payload_start >= slab_size {
                return 0;
            }
            let actual = (slab_size - payload_start) / chunk_size;
            if actual == chunks {
                return chunks;
            }
            chunks = actual;
        }
        chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every class index returns a defined chunk count — non-negative by
    /// type, possibly zero (signalling "needs custom slab size").
    #[test]
    fn chunks_per_slab_defined_for_all_classes() {
        for i in 0..SlabLayout::NUM_CLASSES {
            let _ = SlabLayout::chunks_per_slab(i);
        }
    }

    /// Class 0 (16 B) should give ~3500-4000 chunks in a 64 KiB slab.
    #[test]
    fn class_0_chunks_reasonable() {
        let c = SlabLayout::chunks_per_slab(0);
        assert!(
            (3500..=4096).contains(&c),
            "expected ~3500-4096 chunks for 16-byte class in 64 KiB slab, got {c}",
        );
    }

    /// Class 12 = 64 KiB chunks; a 64 KiB slab can't fit even one
    /// (header + bitmap + chunk > slab_size).  Must report 0 so phase 2
    /// can route through an oversized-slab path.
    #[test]
    fn class_12_needs_custom_slab_size() {
        assert_eq!(
            SlabLayout::chunks_per_slab(12),
            0,
            "class 12 (64 KiB chunks) cannot fit in a 64 KiB default slab",
        );
    }

    /// Small classes (≤ 1 KiB) must always fit at least one chunk.
    #[test]
    fn small_classes_fit_at_least_one_chunk() {
        for i in 0..=6 {
            // 16 B .. 1 KiB
            let c = SlabLayout::chunks_per_slab(i);
            assert!(c > 0, "class {i} ({} B) should fit ≥1 chunk", SlabLayout::class_size(i));
        }
    }

    /// Header math: 32-byte fixed header + ≥1 chunk must fit in 64 KiB.
    #[test]
    fn header_plus_one_chunk_fits() {
        let header = SlabLayout::HDR_FIXED_BYTES as i64;
        let smallest_chunk = SlabLayout::class_size(0) as i64;
        assert!(header + smallest_chunk <= SlabLayout::DEFAULT_SLAB_SIZE);
        assert_eq!(header, 32);
    }

    /// Header offsets are dense, non-overlapping, 8-byte aligned.
    #[test]
    fn header_offsets_dense_and_aligned() {
        assert_eq!(SlabLayout::HDR_CLASS_ID, 0);
        assert_eq!(SlabLayout::HDR_FREE_COUNT, 8);
        assert_eq!(SlabLayout::HDR_NEXT_SLAB, 16);
        assert_eq!(SlabLayout::HDR_PREV_SLAB, 24);
        assert_eq!(SlabLayout::HDR_BITMAP_START, 32);
        assert_eq!(SlabLayout::HDR_BITMAP_START, SlabLayout::HDR_FIXED_BYTES);
    }

    /// Per-class state is 24 B and field offsets are 8-aligned.
    #[test]
    fn class_state_layout() {
        assert_eq!(SlabLayout::CLASS_SLABS_HEAD, 0);
        assert_eq!(SlabLayout::CLASS_CURRENT_SLAB, 8);
        assert_eq!(SlabLayout::CLASS_PADDING, 16);
        assert_eq!(SlabLayout::CLASS_STATE_SIZE, 24);
        // 21 classes × 24 B = 504 B total.
        assert_eq!(
            SlabLayout::CLASS_STATE_SIZE as usize * SlabLayout::NUM_CLASSES,
            504,
        );
    }

    /// Identify (don't fix) the classes that need a custom slab size.
    /// This is the working-set for phase 2.
    #[test]
    fn report_classes_needing_custom_slab_size() {
        let mut needs_custom = Vec::new();
        for i in 0..SlabLayout::NUM_CLASSES {
            if SlabLayout::chunks_per_slab(i) == 0 {
                needs_custom.push((i, SlabLayout::class_size(i)));
            }
        }
        // Sanity: at least the very-large classes (≥ slab_size) must be
        // in this set.
        assert!(!needs_custom.is_empty(),
            "expected at least one class to need a custom slab size");
    }
}
