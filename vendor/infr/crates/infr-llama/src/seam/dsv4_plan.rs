//! DeepSeek V4's per-ubatch compressor plan — the state machine that decides which
//! compressor-state rows a ubatch reads, which cache rows it commits, and which persistent-state
//! ring rows it overwrites. Ported from `dsv4_build_comp_plan` in
//! `llama-kv-cache-dsv4.cpp:427-660`, and specified in prose in `docs/deepseek.md`'s "The
//! compressed-KV state machine" section — read that first, it names the traps this file guards
//! against.
//!
//! This is a PURE function: no graph emission, no backend calls, no I/O. It only computes index
//! and position vectors; nothing here consumes them yet (that is the next slice — wiring
//! `Op::CompressPool` into the V4 graph), so this module has no caller outside its own tests.
//!
//! Scope, deliberately narrower than the reference:
//! - **Single stream only.** infr's V4 is single-stream today, so `dsv4_stream_offset` (which is
//!   `0` whenever `n_stream <= 1`) is dropped entirely rather than carried as dead arithmetic.
//!   [`build_dsv4_comp_plan`] instead takes an explicit `n_seqs` and refuses (mirroring the
//!   reference's own throw) when asked to serve more than one sequence.
//! - **No rollback/`n_rs_seq` planes.** The reference's `state_restore_*`/`state_snapshot_*`
//!   fields exist for speculative-decode rollback; infr's V4 has no consumer for them, so they are
//!   left off [`Dsv4CompPlan`] rather than carried as empty vectors nobody fills.
//! - **No coupled-ubatch CSA branch, and no contiguity guard.** The reference's
//!   `dsv4_ubatch_has_coupled` path pads a coupled multi-sequence ubatch's dummy block
//!   differently, and its non-coupled path refuses a sequence more than one block short of the
//!   ubatch's block count. Single-stream makes that block count a constant `1`, which collapses
//!   the padding rule to "an ubatch that commits nothing gets one dummy" and makes the refusal
//!   unreachable — the derivation, with its citations, sits on the branch itself in
//!   [`build_dsv4_comp_plan`].
//!
//! Landed but unwired, same as `Op::CompressPool` itself was before this slice: the public items
//! below have no caller outside `#[cfg(test)]`, so `dead_code` is silenced deliberately rather
//! than by exporting them somewhere that isn't ready to call them.
#![allow(dead_code)]

use anyhow::{anyhow, Result as AResult};
use std::collections::HashMap;

/// Ratio at which the CSA compressor pools rows. The only ratio for which a non-boundary ubatch
/// gets a synthetic "dummy" commit padded onto the plan — see the CSA branch in
/// [`build_dsv4_comp_plan`]. Mirrors `DSV4_CSA_RATIO` in `llama-kv-cache-dsv4.cpp`.
pub const DSV4_CSA_RATIO: u32 = 4;

/// Ratio at which the HCA compressor pools rows. Never gets the dummy-block padding: the CSA
/// branch is gated on `ratio == DSV4_CSA_RATIO`, so an HCA layer's commit genuinely appears and
/// disappears with the block boundary. Mirrors `DSV4_HCA_RATIO`.
pub const DSV4_HCA_RATIO: u32 = 128;

/// The 256-row graph-width granularity `n_kv` is padded up to, so the compressed-attention graph
/// does not change shape at every block boundary. Mirrors `GGML_PAD(x, 256)`'s `256` in the
/// reference.
const DSV4_N_KV_PAD: usize = 256;

/// The per-ubatch compressor-state recipe: which compressor-state rows this ubatch reads to
/// build compressed rows, which cache rows it commits, and which persistent-state ring rows it
/// overwrites. See `docs/deepseek.md`'s "The per-ubatch plan" for the derivation this mirrors.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Dsv4CompPlan {
    /// Number of completed compressed rows visible to each query token, in ubatch token order
    /// (dense: one entry per token, including padding tokens with `pos < 0`, which get `0`).
    /// `n_visible[i] = (pos + 1) / ratio`, an integer FLOOR — a trailing partial block is
    /// invisible and never committed (see `n_visible`'s derivation in [`build_dsv4_comp_plan`]).
    pub n_visible: Vec<i32>,

    /// Graph-width for the compressed half of attention: `max_i n_visible[i]`, padded up to a
    /// multiple of [`DSV4_N_KV_PAD`]. When no token in the ubatch has completed a block (any
    /// prefill shorter than `ratio` tokens), this stays `0` — and per `docs/deepseek.md`, that is
    /// a DIFFERENT GRAPH (no compressed half at all), not just an all-masked one.
    pub n_kv: usize,

    /// APE row id — `pos % ratio` — for each token with `pos >= 0`, in ubatch token order
    /// (compact: padding tokens are skipped, so this is NOT the same length as `n_visible`).
    pub state_pos: Vec<i32>,

    /// Flattened source row ids for state-backed commits, addressing a graph-local tensor laid
    /// out `[persistent_state | current_ubatch_scratch | sentinel]` (see `state_source_idx` in
    /// [`build_dsv4_comp_plan`] for how a row id is chosen). For an overlapping compressor
    /// (`overlap == true`) this is laid out as **two contiguous halves** — every committed
    /// block's previous-window indices, `ratio` each, followed by every block's current-window
    /// indices, `ratio` each — NOT interleaved per block. For a non-overlapping compressor it is
    /// one `ratio`-sized run per committed block, current-window only.
    pub state_read_idxs: Vec<i32>,

    /// Compressed-cache row ids written by state-backed commits, one per committed block (plus
    /// one synthetic entry for the CSA dummy block, when it applies).
    pub state_write_idxs: Vec<i32>,

    /// RoPE positions for state-backed commits — the committed block's FIRST position
    /// (`pos + 1 - ratio`), which is what the compressed row then ropes at. The CSA dummy
    /// commit's entry is always `0`.
    pub state_write_pos: Vec<i32>,

    /// Current-ubatch token indices to read new compressor state from, one per distinct
    /// persistent-state ring row this ubatch touches. Sorted by, and index-paired with,
    /// [`Self::state_persist_dst_idxs`].
    pub state_persist_src_idxs: Vec<i32>,

    /// Persistent-state ring row ids (`pos % state_size`) this ubatch overwrites, deduplicated —
    /// when several tokens in the ubatch land on the same ring row, only the entry for the
    /// HIGHEST `pos` survives — and sorted ascending so the write order is deterministic.
    pub state_persist_dst_idxs: Vec<i32>,
}

struct PersistRow {
    dst: i32,
    src: i32,
    pos: i32,
}

/// Builds the per-ubatch compressor plan for a single-stream, single-sequence DeepSeek V4 ubatch.
///
/// - `positions`: this ubatch's token positions, in ubatch order; a negative entry marks an
///   unused/padding slot.
/// - `n_seqs`: number of distinct sequences represented in this ubatch. Refused above `1` —
///   infr's V4 is single-stream and has no per-sequence ring offset to serve more (mirrors the
///   reference's `n_stream <= 1 && ubatch.n_seqs_unq > 1` throw, which always applies here since
///   `n_stream` is always `1`).
/// - `ratio`: the compressor's pooling ratio ([`DSV4_CSA_RATIO`] or [`DSV4_HCA_RATIO`]).
/// - `overlap`: whether the compressor pools two `ratio`-sized windows (previous + current) per
///   block, vs. one.
/// - `state_size`: row count of the persistent compressor-state ring.
/// - `kv_size`: row count of the compressed-cache; only its last row (`kv_size - 1`) is
///   addressed here, as the CSA dummy commit's target.
pub fn build_dsv4_comp_plan(
    positions: &[i32],
    n_seqs: u32,
    ratio: u32,
    overlap: bool,
    state_size: u32,
    kv_size: u32,
) -> AResult<Dsv4CompPlan> {
    if n_seqs > 1 {
        return Err(anyhow!(
            "DSV4 single compressed stream cannot serve multiple sequences"
        ));
    }

    let n_tokens = positions.len();
    let ratio_i = ratio as i32;
    let state_size_i = state_size as i32;
    // Single stream: the reference's `state_rows = state_size*n_stream` degenerates to
    // `state_size` exactly, and `dsv4_stream_offset` degenerates to `0` throughout.
    let state_rows = state_size as i64;

    // Last-token-wins position -> ubatch-index map, mirroring the reference's
    // `curr_token_idx_map` build (a later token's assignment overwrites an earlier one at the
    // same position).
    let mut curr_token_idx: HashMap<i32, usize> = HashMap::new();
    for (i, &pos) in positions.iter().enumerate() {
        if pos >= 0 {
            curr_token_idx.insert(pos, i);
        }
    }

    // Addresses the graph-local `[persistent_state | current_ubatch_scratch | sentinel]` tensor:
    // a negative pos is the appended zero/-inf sentinel row; a pos this ubatch itself produced
    // reads that token's own scratch row; anything else reads the persistent ring.
    let state_source_idx = |pos: i32| -> i32 {
        if pos < 0 {
            return (state_rows + n_tokens as i64) as i32;
        }
        if let Some(&i) = curr_token_idx.get(&pos) {
            return (state_rows + i as i64) as i32;
        }
        pos % state_size_i
    };

    let mut n_visible = vec![0i32; n_tokens];
    let mut n_kv: i64 = 0;
    let mut state_pos = Vec::new();
    let mut state_write_idxs = Vec::new();
    let mut state_write_pos = Vec::new();
    let mut state_read_idxs = Vec::new();
    let mut persist_rows: Vec<PersistRow> = Vec::new();

    // The overlap compressor needs its reads as two contiguous halves (see `state_read_idxs`'s
    // doc): collect prev/cur separately per block and concatenate once every block has been
    // visited, rather than interleaving them as each block is processed.
    let mut overlap_prev_reads = Vec::new();
    let mut overlap_cur_reads = Vec::new();

    let mut n_writes: u32 = 0;
    let mut first_valid_token: Option<usize> = None;

    for (i, &pos) in positions.iter().enumerate() {
        if pos < 0 {
            continue;
        }
        if first_valid_token.is_none() {
            first_valid_token = Some(i);
        }

        state_pos.push(pos % ratio_i);

        // FLOOR, not ceiling: a trailing partial block is invisible until a later ubatch
        // completes it. Ceiling this exposes a row built from a half-filled window.
        let visible = (pos as i64 + 1) / ratio as i64;
        n_visible[i] = visible as i32;
        n_kv = n_kv.max(visible);

        // Persist-row dedup: single stream, so the ring destination is just `pos % state_size`.
        // Keep the entry for the highest `pos` when several tokens collide on one ring row.
        let dst = pos % state_size_i;
        match persist_rows.iter_mut().find(|row| row.dst == dst) {
            Some(row) if pos > row.pos => {
                row.src = i as i32;
                row.pos = pos;
            }
            Some(_) => {}
            None => persist_rows.push(PersistRow {
                dst,
                src: i as i32,
                pos,
            }),
        }

        if (pos + 1) % ratio_i != 0 {
            continue;
        }

        // Block boundary: commit cache row `pos / ratio`, roped at the block's FIRST position.
        let source_start = pos + 1 - ratio_i;
        state_write_idxs.push(pos / ratio_i);
        state_write_pos.push(source_start);
        n_writes += 1;

        if overlap {
            let prev_start = source_start - ratio_i;
            for j in 0..ratio_i {
                overlap_prev_reads.push(state_source_idx(prev_start + j));
            }
            for j in 0..ratio_i {
                overlap_cur_reads.push(state_source_idx(source_start + j));
            }
        } else {
            for j in 0..ratio_i {
                state_read_idxs.push(state_source_idx(source_start + j));
            }
        }
    }

    // CSA-only dummy block: an ubatch that commits nothing still gets one synthetic commit, so a
    // non-boundary decode step's graph shape matches a boundary step's. HCA has no such fallback
    // (the reference gates the branch on `ratio == DSV4_CSA_RATIO`); the row is garbage, kept
    // harmless by `n_visible < kv_size` masking it.
    //
    // The reference pads to `ceil(max(1, ubatch.n_seq_tokens)/ratio)` blocks and refuses a
    // sequence more than one block short of that ("DSV4 CSA sequence positions are not
    // contiguous"). Neither carries over here, and not because they were skipped: for a
    // single-stream cache — infr's only supported scope — that block count is ALWAYS `1`.
    // `llama_kv_cache_dsv4::init_batch` takes the `split_simple` branch whenever
    // `raw_per_seq || comp_per_seq` is false, which single-stream makes it
    // (`llama-kv-cache-dsv4.cpp:1292-1293`), and `split_simple` ends in
    // `ubatch_add(idxs, idxs.size(), false)` — passing `n_seqs == n_tokens`, so
    // `n_seq_tokens = n_tokens/n_seqs` is unconditionally `1` (`llama-batch.cpp:507,821`).
    // With `n_blocks == 1` the guard's `n_writes + 1 != n_blocks` can only be reached when
    // `n_writes == 0`, where it holds trivially — an unreachable refusal, so it is not ported.
    if ratio == DSV4_CSA_RATIO && n_writes == 0 && !state_pos.is_empty() {
        debug_assert!(kv_size > 0, "DSV4 CSA dummy block needs a non-empty cache");

        let i = first_valid_token.expect("state_pos non-empty implies at least one valid token");
        let source_idx = state_source_idx(positions[i]);

        state_write_idxs.push(kv_size as i32 - 1);
        state_write_pos.push(0);

        if overlap {
            for _ in 0..ratio {
                overlap_prev_reads.push(source_idx);
                overlap_cur_reads.push(source_idx);
            }
        } else {
            for _ in 0..ratio {
                state_read_idxs.push(source_idx);
            }
        }
    }

    if overlap {
        // [ all blocks' prev-window indices | all blocks' cur-window indices ]
        state_read_idxs.reserve(overlap_prev_reads.len() + overlap_cur_reads.len());
        state_read_idxs.extend(overlap_prev_reads);
        state_read_idxs.extend(overlap_cur_reads);
    }

    let n_kv = (n_kv as usize).div_ceil(DSV4_N_KV_PAD) * DSV4_N_KV_PAD;

    persist_rows.sort_by_key(|row| row.dst);
    let state_persist_src_idxs = persist_rows.iter().map(|row| row.src).collect();
    let state_persist_dst_idxs = persist_rows.iter().map(|row| row.dst).collect();

    Ok(Dsv4CompPlan {
        n_visible,
        n_kv,
        state_pos,
        state_read_idxs,
        state_write_idxs,
        state_write_pos,
        state_persist_src_idxs,
        state_persist_dst_idxs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 12-token CSA prefill, ratio 4, overlap, `state_size = 8`, positions `0..=11`. Commits
    /// happen at `pos` 3, 7, 11 (block boundaries), giving three full blocks — enough to exercise
    /// the sentinel path (block 0's previous window), the ring-wrap persist dedup (12 tokens >
    /// `state_size` 8), and the two-halves overlap read order, all with vectors derived by hand
    /// below rather than asserted only on length.
    ///
    /// Derivation (ratio=4, state_size=8, kv_size=16, n_tokens=12, positions[i] == i):
    /// - `state_pos[i] = pos % 4` for all 12 tokens: `[0,1,2,3, 0,1,2,3, 0,1,2,3]`.
    /// - `n_visible[i] = (pos+1)/4` floor: `[0,0,0,1, 1,1,1,2, 2,2,2,3]`; max is 3, padded to 256.
    /// - Commits at pos 3 (source_start=0, write_idx=0, write_pos=0), pos 7 (source_start=4,
    ///   write_idx=1, write_pos=4), pos 11 (source_start=8, write_idx=2, write_pos=8).
    /// - `state_source_idx`: since `positions[i] == i`, any pos in `0..=11` is this ubatch's own
    ///   token i, giving `state_rows(8) + i`. A pos `< 0` gives the sentinel `8 + 12 = 20`.
    ///   - block pos=3: prev_start=-4 -> reads -4,-3,-2,-1, all `< 0` -> sentinel `[20,20,20,20]`.
    ///     cur reads 0,1,2,3 -> `[8,9,10,11]`.
    ///   - block pos=7: prev_start=0 -> reads 0,1,2,3 -> `[8,9,10,11]` (this ubatch's own rows).
    ///     cur reads 4,5,6,7 -> `[12,13,14,15]`.
    ///   - block pos=11: prev_start=4 -> reads 4,5,6,7 -> `[12,13,14,15]`.
    ///     cur reads 8,9,10,11 -> `[16,17,18,19]`.
    ///   - `n_blocks = ceil(12/4) = 3 == n_writes`, so no CSA dummy block.
    ///   - two-halves concat: prev (12) then cur (12), 24 entries total.
    /// - Persist ring dedup (`dst = pos % 8`): pos 8..11 (indices 8..11) overwrite pos 0..3
    ///   (indices 0..3) at the same ring rows since 8 > 0 etc; pos 4..7 (indices 4..7) are
    ///   untouched. Sorted by dst 0..7: `src = [8,9,10,11, 4,5,6,7]`.
    #[test]
    fn prefill_commits_several_csa_blocks_overlap() {
        let positions: Vec<i32> = (0..12).collect();
        let plan = build_dsv4_comp_plan(&positions, 1, DSV4_CSA_RATIO, true, 8, 16).unwrap();

        assert_eq!(plan.state_pos, vec![0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3]);
        assert_eq!(plan.n_visible, vec![0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3]);
        assert_eq!(plan.n_kv, 256);

        assert_eq!(plan.state_write_idxs, vec![0, 1, 2]);
        assert_eq!(plan.state_write_pos, vec![0, 4, 8]);

        let expected_reads = vec![
            // prev half: block0 (sentinel), block1, block2
            20, 20, 20, 20, 8, 9, 10, 11, 12, 13, 14, 15,
            // cur half: block0, block1, block2
            8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
        ];
        assert_eq!(plan.state_read_idxs, expected_reads);

        assert_eq!(plan.state_persist_src_idxs, vec![8, 9, 10, 11, 4, 5, 6, 7]);
        assert_eq!(plan.state_persist_dst_idxs, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    /// Isolates the overlap two-halves order on its own (smaller than the big prefill test, and
    /// starting mid-stream so the previous window is read from the persistent RING rather than
    /// the sentinel, exercising the other branch of `state_source_idx`).
    ///
    /// Derivation (ratio=4, overlap, state_size=8, positions `4..=11`, so ubatch index i has
    /// pos = 4+i):
    /// - Commits at pos 7 (i=3: source_start=4, write_idx=1, write_pos=4) and pos 11 (i=7:
    ///   source_start=8, write_idx=2, write_pos=8).
    /// - block pos=7: prev_start=0 -> reads pos 0,1,2,3, none in this ubatch -> ring `pos % 8` ->
    ///   `[0,1,2,3]`. cur reads pos 4,5,6,7 -> this ubatch's i=0..3 -> `state_rows(8)+i` ->
    ///   `[8,9,10,11]`.
    /// - block pos=11: prev_start=4 -> reads pos 4,5,6,7 -> this ubatch's i=0..3 -> `[8,9,10,11]`
    ///   (same values as block0's cur half — expected, both address the same rows). cur reads pos
    ///   8,9,10,11 -> this ubatch's i=4..7 -> `[12,13,14,15]`.
    /// - Two-halves concat: `[0,1,2,3, 8,9,10,11]` (prev) then `[8,9,10,11, 12,13,14,15]` (cur).
    ///   An interleaved implementation would instead produce
    ///   `[0,1,2,3, 8,9,10,11, 8,9,10,11, 12,13,14,15]` in per-block prev/cur/prev/cur order,
    ///   which happens to coincide here in the middle four entries but diverges at the front and
    ///   back — this is why the big prefill test's sentinel-filled block0 is the sharper check.
    #[test]
    fn overlap_two_halves_are_not_interleaved() {
        let positions: Vec<i32> = (4..12).collect();
        let plan = build_dsv4_comp_plan(&positions, 1, DSV4_CSA_RATIO, true, 8, 16).unwrap();

        assert_eq!(plan.state_write_idxs, vec![1, 2]);
        assert_eq!(
            plan.state_read_idxs,
            vec![0, 1, 2, 3, 8, 9, 10, 11, 8, 9, 10, 11, 12, 13, 14, 15]
        );
    }

    /// Block 0's previous window is entirely before the sequence start (`pos < 0`), so all
    /// `ratio` of its prev-half reads must be the sentinel row `state_rows + n_tokens`.
    #[test]
    fn block_zero_prev_window_is_all_sentinel() {
        let positions: Vec<i32> = vec![0, 1, 2, 3];
        let state_size = 8u32;
        let plan =
            build_dsv4_comp_plan(&positions, 1, DSV4_CSA_RATIO, true, state_size, 16).unwrap();

        let sentinel = state_size as i32 + positions.len() as i32;
        assert_eq!(plan.state_read_idxs[0..4], vec![sentinel; 4]);
        // cur half: pos 0..3 are this ubatch's own tokens i=0..3 -> state_size + i.
        assert_eq!(
            plan.state_read_idxs[4..8],
            vec![
                state_size as i32,
                state_size as i32 + 1,
                state_size as i32 + 2,
                state_size as i32 + 3
            ]
        );
    }

    /// A single-token CSA decode step off a block boundary: no real commit (pos=13,
    /// `(13+1) % 4 == 2 != 0`), but the CSA dummy block IS appended so this step's graph matches
    /// a boundary step's. Uses `overlap = false` to exercise that branch's inline (non-two-halves)
    /// read layout.
    #[test]
    fn decode_step_csa_appends_dummy_block() {
        let positions: Vec<i32> = vec![13];
        let state_size = 8u32;
        let kv_size = 16u32;
        let plan = build_dsv4_comp_plan(&positions, 1, DSV4_CSA_RATIO, false, state_size, kv_size)
            .unwrap();

        assert_eq!(plan.state_write_idxs, vec![kv_size as i32 - 1]);
        assert_eq!(plan.state_write_pos, vec![0]);
        // pos 13 is this ubatch's own token i=0 -> state_size + 0.
        assert_eq!(plan.state_read_idxs, vec![state_size as i32; 4]);
        assert_eq!(plan.state_pos, vec![13 % 4]);
    }

    /// The HCA counterpart of the above: a single-token decode step off a block boundary gets
    /// no commit and, because the CSA branch is gated on `ratio == DSV4_CSA_RATIO`, no dummy
    /// block either. Also doubles as the `n_kv == 0` short-prefill case: `(50+1)/128` floors to
    /// `0`, and `GGML_PAD(0, 256) == 0`.
    #[test]
    fn decode_step_hca_has_no_commit_and_no_dummy() {
        let positions: Vec<i32> = vec![50];
        let plan = build_dsv4_comp_plan(&positions, 1, DSV4_HCA_RATIO, false, 128, 32).unwrap();

        assert!(plan.state_write_idxs.is_empty());
        assert!(plan.state_write_pos.is_empty());
        assert!(plan.state_read_idxs.is_empty());
        assert_eq!(plan.n_visible, vec![0]);
        assert_eq!(plan.n_kv, 0);
    }

    /// A trailing partial block at the end of a longer CSA prefill: positions 0..=5 complete one
    /// block (pos 3) but leave pos 4,5 as an incomplete second block. `n_visible` must floor —
    /// tokens 4 and 5 stay at the SAME `n_visible` as token 3 (`1`), not `2` — and that partial
    /// block is never committed. The dummy does NOT appear either: this ubatch already made a
    /// real commit, and the padding exists only to give a commit-less ubatch one.
    #[test]
    fn n_visible_floors_at_a_trailing_partial_block() {
        let positions: Vec<i32> = vec![0, 1, 2, 3, 4, 5];
        let plan = build_dsv4_comp_plan(&positions, 1, DSV4_CSA_RATIO, false, 8, 16).unwrap();

        assert_eq!(plan.n_visible, vec![0, 0, 0, 1, 1, 1]);
        assert_eq!(plan.state_write_idxs, vec![0]); // real commit only: pos/ratio = 3/4 = 0
        assert_eq!(plan.state_write_pos, vec![0]);
    }

    /// Persist-row dedup under ring wrap: more tokens (12) than `state_size` (8), so positions
    /// 8..11 land on the same ring rows as 0..3. The surviving `src` must be the HIGHER-pos
    /// token's index, and destinations must come out sorted. (This is the same ubatch as
    /// `prefill_commits_several_csa_blocks_overlap`; isolated here so the persist assertion
    /// stands on its own.)
    #[test]
    fn persist_dedup_keeps_highest_pos_and_sorts_by_dst() {
        let positions: Vec<i32> = (0..12).collect();
        let plan = build_dsv4_comp_plan(&positions, 1, DSV4_CSA_RATIO, true, 8, 16).unwrap();

        assert_eq!(plan.state_persist_dst_idxs, vec![0, 1, 2, 3, 4, 5, 6, 7]);
        // dst 0..3 are won by tokens 8..11 (higher pos); dst 4..7 only ever touched by 4..7.
        assert_eq!(plan.state_persist_src_idxs, vec![8, 9, 10, 11, 4, 5, 6, 7]);
    }

    /// Mirrors the reference's `"DSV4 single compressed stream cannot serve multiple sequences"`
    /// throw: infr's V4 is single-stream, so more than one sequence in a ubatch must be refused
    /// rather than silently addressing the wrong ring rows.
    #[test]
    fn refuses_more_than_one_sequence() {
        let err = build_dsv4_comp_plan(&[0, 1, 2, 3], 2, DSV4_CSA_RATIO, true, 8, 16).unwrap_err();
        assert!(
            err.to_string().contains("single compressed stream"),
            "unexpected error: {err}"
        );
    }

    /// A multi-token CSA ubatch that commits NOTHING gets exactly ONE dummy, not one per block
    /// the token count implies. 9 tokens whose absolute positions never land on a `%4 == 3`
    /// boundary (0-2, 100-102, 200-202), so no block completes. The reference's block count is
    /// `1` for every single-stream ubatch regardless of its token count (see the branch's own
    /// derivation), so this pads once — a port reading it as `ceil(9/4) == 3` instead refuses
    /// this ubatch outright.
    ///
    /// The reads are all the same repeated source: pos 0 is this ubatch's own token `i == 0`, so
    /// `state_source_idx` returns the scratch row `state_size + 0`, and the overlapping
    /// compressor repeats it across both halves.
    #[test]
    fn csa_ubatch_with_no_commits_pads_exactly_one_dummy() {
        let positions: Vec<i32> = vec![0, 1, 2, 100, 101, 102, 200, 201, 202];
        let state_size = 8u32;
        let kv_size = 16u32;
        let plan =
            build_dsv4_comp_plan(&positions, 1, DSV4_CSA_RATIO, true, state_size, kv_size).unwrap();

        assert_eq!(plan.state_write_idxs, vec![kv_size as i32 - 1]);
        assert_eq!(plan.state_write_pos, vec![0]);
        assert_eq!(
            plan.state_read_idxs,
            vec![state_size as i32; 2 * DSV4_CSA_RATIO as usize]
        );
    }
}
