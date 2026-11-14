import Std

/- Models of timestamp decisions in Db::replay_record and the time-pruning
   part of Db::query/aggregate. Timestamps are mathematical integers. -/
namespace Storage

def replaySuffix (watermark : Int) (record : List Int) : List Int :=
  record.dropWhile (· ≤ watermark)

-- For a strictly ordered record, partitioning at the watermark is exactly
-- filtering out timestamps already represented by the snapshot.
theorem replay_suffix_eq_filter (record : List Int) (watermark : Int)
    (ordered : record.Pairwise (· < ·)) :
    replaySuffix watermark record = record.filter (watermark < ·) := by
  induction record with
  | nil => simp [replaySuffix]
  | cons x xs ih =>
    have hs := List.pairwise_cons.mp ordered
    by_cases hx : x ≤ watermark
    · simpa [replaySuffix, hx] using ih hs.2
    · have hall : ∀ y ∈ xs, watermark < y := by
        intro y hy
        have := hs.1 y hy
        omega
      have hf : xs.filter (watermark < ·) = xs := by
        apply List.filter_eq_self.mpr
        intro y hy
        simp [hall y hy]
      simp [replaySuffix, hx, hf, show watermark < x by omega]

-- Once the resulting snapshot's watermark reaches the record's maximum,
-- replaying that record adds no points, including when it overlaps a snapshot.
theorem replay_again_empty (record : List Int) (watermark : Int)
    (covered : ∀ t ∈ record, t ≤ watermark) :
    replaySuffix watermark record = [] := by
  unfold replaySuffix
  induction record with
  | nil => simp
  | cons x xs ih =>
    have hx : x ≤ watermark := covered x (by simp)
    have ht : ∀ t ∈ xs, t ≤ watermark := by
      intro t ht
      exact covered t (by simp [ht])
    simp [hx, ih ht]

-- The last point of a nonempty strictly increasing record covers every
-- earlier point. Thus applying its nonempty suffix sets a watermark that
-- makes the next replay a no-op.
theorem replay_at_record_end_empty (earlier : List Int) (last : Int)
    (ordered : (earlier ++ [last]).Pairwise (· < ·)) :
    replaySuffix last (earlier ++ [last]) = [] := by
  apply replay_again_empty
  intro t ht
  have hp := List.pairwise_append.mp ordered
  simp only [List.mem_append, List.mem_singleton] at ht
  rcases ht with ht | rfl
  · have h := hp.2.2 t ht last (by simp)
    omega
  · omega

-- A point enclosed by honest block metadata cannot satisfy the query if
-- the block is wholly before or wholly after the inclusive query interval.
theorem before_block_has_no_match (t min max start finish : Int)
    (bounds : min ≤ t ∧ t ≤ max) (before : max < start) :
    ¬ (start ≤ t ∧ t ≤ finish) := by omega

theorem after_block_has_no_match (t min max start finish : Int)
    (bounds : min ≤ t ∧ t ≤ max) (after : finish < min) :
    ¬ (start ≤ t ∧ t ≤ finish) := by omega

-- With ordered, non-overlapping blocks, a later block must also be after
-- the query once the first after-query block triggers the loop's break.
theorem break_skips_later_block (finish min₁ max₁ min₂ : Int)
    (after : finish < min₁) (valid : min₁ ≤ max₁)
    (nonoverlap : max₁ < min₂) : finish < min₂ := by omega

-- Cached projection replaces only the changed series' contribution. Natural
-- subtraction agrees with Rust's saturating subtraction under the cache bound.
theorem replace_cached_contribution (other before after : Nat) :
    (other + before) - before + after = other + after := by omega

#print axioms replay_suffix_eq_filter
#print axioms replay_again_empty
#print axioms replay_at_record_end_empty
#print axioms before_block_has_no_match
#print axioms after_block_has_no_match
#print axioms break_skips_later_block
#print axioms replace_cached_contribution
end Storage
