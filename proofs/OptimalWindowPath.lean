import Std

/- A finite-horizon model of the Gorilla-RLE value stream's canonical-window
   grammar. A fresh transition installs the tight window of this residual;
   reuse is available only when the current window contains the residual.
   The concrete Rust implementation computes this recurrence forward and
   stores predecessors. This model does not specify Rust byte IO. -/

structure WindowStep (State : Type) where
  freshCost : Nat
  freshState : State
  reuseCost : State → Option Nat

def optimalCost {State : Type} : List (WindowStep State) → State → Nat
  | [], _ => 0
  | step :: rest, state =>
    let fresh := step.freshCost + optimalCost rest step.freshState
    match step.reuseCost state with
    | none => fresh
    | some reuse => min fresh (reuse + optimalCost rest state)

def planCost {State : Type} : List (WindowStep State) → State → List Bool → Option Nat
  | [], _, [] => some 0
  | [], _, _ :: _ => none
  | _ :: _, _, [] => none
  | step :: rest, state, fresh :: choices =>
    if fresh then
      (planCost rest step.freshState choices).map (step.freshCost + ·)
    else
      (step.reuseCost state).bind fun reuse =>
        (planCost rest state choices).map (reuse + ·)

theorem optimal_le_valid_plan {State : Type} (steps : List (WindowStep State)) (state : State)
    (choices : List Bool) (cost : Nat)
    (h : planCost steps state choices = some cost) :
    optimalCost steps state ≤ cost := by
  induction steps generalizing state choices cost with
  | nil =>
    cases choices with
    | nil => simp [planCost, optimalCost] at h ⊢
    | cons _ _ => simp [planCost] at h
  | cons step rest ih =>
    cases choices with
    | nil => simp [planCost] at h
    | cons fresh tail =>
      cases fresh with
      | true =>
        simp [planCost] at h
        cases htail : planCost rest step.freshState tail with
        | none => simp [htail] at h
        | some tailCost =>
          simp [htail] at h
          subst cost
          have bound := ih step.freshState tail tailCost htail
          simp only [optimalCost]
          split <;> omega
      | false =>
        simp [planCost] at h
        cases hre : step.reuseCost state with
        | none => simp [hre] at h
        | some reuse =>
          cases htail : planCost rest state tail with
          | none => simp [hre, htail] at h
          | some tailCost =>
            simp [hre, htail] at h
            subst cost
            have bound := ih state tail tailCost htail
            simp [optimalCost, hre]
            omega

theorem optimal_attained {State : Type} (steps : List (WindowStep State)) (state : State) :
    ∃ choices, planCost steps state choices = some (optimalCost steps state) := by
  induction steps generalizing state with
  | nil => exact ⟨[], rfl⟩
  | cons step rest ih =>
    obtain ⟨freshChoices, hfresh⟩ := ih step.freshState
    cases hre : step.reuseCost state with
    | none =>
      refine ⟨true :: freshChoices, ?_⟩
      simp [planCost, optimalCost, hre, hfresh]
    | some reuse =>
      obtain ⟨reuseChoices, hreuse⟩ := ih state
      by_cases hle : step.freshCost + optimalCost rest step.freshState ≤
          reuse + optimalCost rest state
      · refine ⟨true :: freshChoices, ?_⟩
        simp [planCost, optimalCost, hre, hfresh]
        omega
      · refine ⟨false :: reuseChoices, ?_⟩
        have hright : reuse + optimalCost rest state ≤
            step.freshCost + optimalCost rest step.freshState := by omega
        simp [planCost, optimalCost, hre, hreuse]
        omega

/- A nonzero XOR has canonical tight window (lz,tz), with lz capped at 31.
   A zero XOR costs one bit and preserves the state, so omitting it from the
   step list changes every plan cost by the same constant. The caller supplies
   actual nonzero residuals satisfying lz + tz < 64. -/
structure CanonicalWindow where
  lz : Nat
  tz : Nat
  lz_le : lz ≤ 31
  width_pos : lz + tz < 64

def canonicalStep (window : CanonicalWindow) : WindowStep (Option CanonicalWindow) where
  freshCost := 13 + (64 - window.lz - window.tz)
  freshState := some window
  reuseCost := fun state =>
    match state with
    | none => none
    | some old =>
      if old.lz ≤ window.lz ∧ old.tz ≤ window.tz then
        some (2 + (64 - old.lz - old.tz))
      else none

theorem canonical_optimal_attained (windows : List CanonicalWindow) :
    ∃ choices, planCost (windows.map canonicalStep) none choices =
      some (optimalCost (windows.map canonicalStep) none) :=
  optimal_attained _ _

theorem canonical_optimal_le_valid_plan (windows : List CanonicalWindow)
    (choices : List Bool) (cost : Nat)
    (h : planCost (windows.map canonicalStep) none choices = some cost) :
    optimalCost (windows.map canonicalStep) none ≤ cost :=
  optimal_le_valid_plan _ _ _ _ h

/- The existing greedy policy is a concrete baseline, not an assumed valid
   competitor: reuse exactly when eligible and no dearer than fresh, including
   ties. Costs include both tag bits, so comparing them is equivalent to the
   Rust comparison old_width ≤ 11 + new_width. -/
def greedyChoices {State : Type} : List (WindowStep State) → State → List Bool
  | [], _ => []
  | step :: rest, state =>
    match step.reuseCost state with
    | none => true :: greedyChoices rest step.freshState
    | some reuse =>
      if reuse ≤ step.freshCost then false :: greedyChoices rest state
      else true :: greedyChoices rest step.freshState

def greedyCost {State : Type} : List (WindowStep State) → State → Nat
  | [], _ => 0
  | step :: rest, state =>
    match step.reuseCost state with
    | none => step.freshCost + greedyCost rest step.freshState
    | some reuse =>
      if reuse ≤ step.freshCost then reuse + greedyCost rest state
      else step.freshCost + greedyCost rest step.freshState

theorem greedy_plan_valid {State : Type} (steps : List (WindowStep State)) (state : State) :
    planCost steps state (greedyChoices steps state) = some (greedyCost steps state) := by
  induction steps generalizing state with
  | nil => rfl
  | cons step rest ih =>
    cases hre : step.reuseCost state with
    | none => simp [greedyChoices, greedyCost, planCost, hre, ih]
    | some reuse =>
      by_cases hle : reuse ≤ step.freshCost
      · simp [greedyChoices, greedyCost, planCost, hre, hle, ih]
      · simp [greedyChoices, greedyCost, planCost, hre, hle, ih]

-- No plan-validity or desired cost inequality is assumed from the caller.
theorem canonical_no_regression_bits (windows : List CanonicalWindow) :
    optimalCost (windows.map canonicalStep) none ≤
      greedyCost (windows.map canonicalStep) none :=
  optimal_le_valid_plan _ _ _ _ (greedy_plan_valid _ _)

/- The production optimizer falls back to greedy above 2048 points. Count
   includes the first point and zero residuals, unlike `windows`. Keeping it
   explicit avoids treating the number of nonzero residuals as the cap. -/
def cappedCanonicalCost (pointCount : Nat) (windows : List CanonicalWindow) : Nat :=
  if pointCount ≤ 2048 then optimalCost (windows.map canonicalStep) none
  else greedyCost (windows.map canonicalStep) none

theorem capped_canonical_no_regression_bits (pointCount : Nat)
    (windows : List CanonicalWindow) :
    cappedCanonicalCost pointCount windows ≤ greedyCost (windows.map canonicalStep) none := by
  unfold cappedCanonicalCost
  split
  · exact canonical_no_regression_bits windows
  · exact Nat.le_refl _

-- Shared framing (first value and timestamp stream) and one bit per omitted
-- zero XOR are explicit. Padding to whole bytes preserves the bit bound.
theorem canonical_no_regression_bytes (pointCount framingBits zeroCount : Nat)
    (windows : List CanonicalWindow) :
    (framingBits + zeroCount + cappedCanonicalCost pointCount windows + 7) / 8 ≤
      (framingBits + zeroCount + greedyCost (windows.map canonicalStep) none + 7) / 8 := by
  have bound := capped_canonical_no_regression_bits pointCount windows
  omega

#print axioms optimal_le_valid_plan
#print axioms optimal_attained
#print axioms canonical_optimal_attained
#print axioms canonical_optimal_le_valid_plan
#print axioms greedy_plan_valid
#print axioms canonical_no_regression_bits
#print axioms capped_canonical_no_regression_bits
#print axioms canonical_no_regression_bytes
