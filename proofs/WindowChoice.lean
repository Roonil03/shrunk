import Std

/- A model of the compatible-window branch of encode_xor. The common two tag
   bits are omitted; the new-window metadata is 5 + 6 bits. -/
def windowCost (oldWidth newWidth : Nat) : Nat :=
  if oldWidth ≤ 11 + newWidth then oldWidth else 11 + newWidth

theorem chosen_le_reuse (oldWidth newWidth : Nat) :
    windowCost oldWidth newWidth ≤ oldWidth := by
  unfold windowCost
  split <;> omega

theorem chosen_le_new (oldWidth newWidth : Nat) :
    windowCost oldWidth newWidth ≤ 11 + newWidth := by
  unfold windowCost
  split <;> omega

theorem ties_reuse (oldWidth newWidth : Nat)
    (h : oldWidth = 11 + newWidth) :
    windowCost oldWidth newWidth = oldWidth := by
  simp [windowCost, h]

/- XOR reconstructs each bit of the current value from the previous bit and
   its encoded residual. This does not model window packing or Rust IO. -/
theorem xor_reconstruct (previous current : Bool) :
    Bool.xor previous (Bool.xor previous current) = current := by
  cases previous <;> cases current <;> decide

-- A full-width residual is reversed by the same XOR operation.
theorem xor64_reconstruct (previous current : BitVec 64) :
    previous ^^^ (previous ^^^ current) = current := by
  rw [← BitVec.xor_assoc]
  simp

-- The payload contains the low `width` bits after removing `trailing`
-- zero bits. The two hypotheses express the window's zero regions.
def packWindow (x : BitVec 64) (trailing width : Nat) : BitVec width :=
  (x >>> trailing).setWidth width

def unpackWindow (payload : BitVec width) (trailing : Nat) : BitVec 64 :=
  payload.setWidth 64 <<< trailing

theorem window_roundtrip (x : BitVec 64) (trailing width : Nat)
    (low : x &&& (BitVec.allOnes 64 <<< trailing) = x)
    (high : (x >>> trailing).toNat < 2 ^ width) :
    unpackWindow (packWindow x trailing width) trailing = x := by
  unfold packWindow unpackWindow
  have h : ((x >>> trailing).setWidth width).setWidth 64 = x >>> trailing := by
    apply BitVec.eq_of_toNat_eq
    simp only [BitVec.toNat_setWidth]
    rw [Nat.mod_eq_of_lt high, Nat.mod_eq_of_lt (x >>> trailing).isLt]
  rw [h, BitVec.shiftLeft_ushiftRight, low]

#print axioms xor64_reconstruct
#print axioms window_roundtrip
#print axioms chosen_le_reuse
#print axioms chosen_le_new

-- A stateful stream model: each residual is relative to the preceding value.
def encodeResiduals (previous : BitVec 64) : List (BitVec 64) → List (BitVec 64)
  | [] => []
  | current :: rest => (previous ^^^ current) :: encodeResiduals current rest

def decodeResiduals (previous : BitVec 64) : List (BitVec 64) → List (BitVec 64)
  | [] => []
  | residual :: rest =>
    let current := previous ^^^ residual
    current :: decodeResiduals current rest

theorem residuals_roundtrip (previous : BitVec 64) (values : List (BitVec 64)) :
    decodeResiduals previous (encodeResiduals previous values) = values := by
  induction values generalizing previous with
  | nil => rfl
  | cons current rest ih =>
    simp [encodeResiduals, decodeResiduals, xor64_reconstruct, ih]

#print axioms residuals_roundtrip
