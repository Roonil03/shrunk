import Std

/- Block::build starts at Raw's 16 bytes per point and retains a candidate
   only on strict improvement. A slot is None when gated out or inapplicable;
   Some records the already encoded candidate's byte length. This model counts
   size comparisons only, not encoding, gate scans, allocation or CPU time. -/
def selectSizes : Nat → List (Option Nat) → Nat × Nat
  | best, [] => (best, 0)
  | best, none :: rest => selectSizes best rest
  | best, some size :: rest =>
    let result := selectSizes (if size < best then size else best) rest
    (result.1, result.2 + 1)

theorem selected_le_initial (best : Nat) (slots : List (Option Nat)) :
    (selectSizes best slots).1 ≤ best := by
  induction slots generalizing best with
  | nil => simp [selectSizes]
  | cons slot rest ih =>
    cases slot with
    | none => exact ih best
    | some size =>
      by_cases smaller : size < best
      · simpa [selectSizes, smaller] using Nat.le_trans (ih size) (Nat.le_of_lt smaller)
      · simpa [selectSizes, smaller] using ih best

theorem selected_le_admitted (best size : Nat) (slots : List (Option Nat))
    (admitted : some size ∈ slots) :
    (selectSizes best slots).1 ≤ size := by
  induction slots generalizing best with
  | nil => simp at admitted
  | cons slot rest ih =>
    cases slot with
    | none =>
      simp only [List.mem_cons, reduceCtorEq, false_or] at admitted
      exact ih best admitted
    | some current =>
      simp only [List.mem_cons, Option.some.injEq] at admitted
      rcases admitted with same | tail
      · subst current
        by_cases smaller : size < best
        · simpa [selectSizes, smaller] using selected_le_initial size rest
        · have bound := selected_le_initial best rest
          simp only [selectSizes, if_neg smaller]
          omega
      · exact ih (if current < best then current else best) tail

def admittedCount : List (Option Nat) → Nat
  | [] => 0
  | none :: rest => admittedCount rest
  | some _ :: rest => admittedCount rest + 1

theorem selection_comparisons_exact (best : Nat) (slots : List (Option Nat)) :
    (selectSizes best slots).2 = admittedCount slots := by
  induction slots generalizing best with
  | nil => rfl
  | cons slot rest ih =>
    cases slot <;> simp [selectSizes, admittedCount, ih]

theorem selection_comparisons_le_slots (best : Nat) (slots : List (Option Nat)) :
    (selectSizes best slots).2 ≤ slots.length := by
  rw [selection_comparisons_exact]
  induction slots with
  | nil => simp [admittedCount]
  | cons slot rest ih =>
    cases slot <;> simp [admittedCount] <;> omega

-- The six slots match Codec::CANDIDATES. No assumptions about their sizes or
-- gates are needed. Skipped candidates have no claimed size bound.
theorem block_selection_bounds (points : Nat)
    (gr dolphin predictor gorilla delta rle : Option Nat) :
    (selectSizes (16 * points) [gr, dolphin, predictor, gorilla, delta, rle]).1 ≤
        16 * points ∧
    (selectSizes (16 * points) [gr, dolphin, predictor, gorilla, delta, rle]).2 ≤ 6 := by
  constructor
  · exact selected_le_initial _ _
  · exact selection_comparisons_le_slots _ _

#print axioms selected_le_initial
#print axioms selected_le_admitted
#print axioms selection_comparisons_exact
#print axioms selection_comparisons_le_slots
#print axioms block_selection_bounds
