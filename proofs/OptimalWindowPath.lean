import Std

/- A finite-horizon model of the Gorilla-RLE value stream's canonical-window
   grammar. A fresh transition installs the tight window of this residual;
   reuse is available only when the current window contains the residual.
   The concrete Rust implementation computes this recurrence forward and
   stores predecessors. This model does not specify Rust byte IO. -/

structure WindowStep where
  freshCost : Nat
  freshState : Nat
  reuseCost : Nat → Option Nat

def optimalCost : List WindowStep → Nat → Nat
  | [], _ => 0
  | step :: rest, state =>
    let fresh := step.freshCost + optimalCost rest step.freshState
    match step.reuseCost state with
    | none => fresh
    | some reuse => min fresh (reuse + optimalCost rest state)

def planCost : List WindowStep → Nat → List Bool → Option Nat
  | [], _, [] => some 0
  | [], _, _ :: _ => none
  | _ :: _, _, [] => none
  | step :: rest, state, fresh :: choices =>
    if fresh then
      (planCost rest step.freshState choices).map (step.freshCost + ·)
    else
      (step.reuseCost state).bind fun reuse =>
        (planCost rest state choices).map (reuse + ·)

theorem optimal_le_valid_plan (steps : List WindowStep) (state : Nat)
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

#print axioms optimal_le_valid_plan
