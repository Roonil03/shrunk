import Std

-- Two's-complement bit patterns, corresponding to codec.rs's i64/u64 casts.
def zigzag64 (x : BitVec 64) : BitVec 64 :=
  (x <<< 1) ^^^ x.sshiftRight 63

def unzigzag64 (x : BitVec 64) : BitVec 64 :=
  (x >>> 1) ^^^ (-(x &&& 1))

theorem unzigzag_zigzag64 (x : BitVec 64) :
    unzigzag64 (zigzag64 x) = x := by
  unfold unzigzag64 zigzag64
  bv_decide

theorem zigzag_unzigzag64 (x : BitVec 64) :
    zigzag64 (unzigzag64 x) = x := by
  unfold unzigzag64 zigzag64
  bv_decide

#print axioms unzigzag_zigzag64
#print axioms zigzag_unzigzag64

-- The writer shifts left before adding each next bit. After eight writes its
-- accumulator is a byte, with the first bit in position 7.
def bitByte (b : Bool) : BitVec 8 := if b then 1 else 0

def packByte (a b c d e f g h : Bool) : BitVec 8 :=
  [a,b,c,d,e,f,g,h].foldl (fun acc bit => (acc <<< 1) ||| bitByte bit) 0

def byteBits (x : BitVec 8) : List Bool :=
  [x.getLsbD 7, x.getLsbD 6, x.getLsbD 5, x.getLsbD 4,
   x.getLsbD 3, x.getLsbD 2, x.getLsbD 1, x.getLsbD 0]

theorem byte_bits_pack (a b c d e f g h : Bool) :
    byteBits (packByte a b c d e f g h) = [a,b,c,d,e,f,g,h] := by
  unfold byteBits packByte bitByte
  cases a <;> cases b <;> cases c <;> cases d <;> cases e <;>
    cases f <;> cases g <;> cases h <;> decide

theorem pack_byte_bits (x : BitVec 8) :
    packByte (x.getLsbD 7) (x.getLsbD 6) (x.getLsbD 5) (x.getLsbD 4)
      (x.getLsbD 3) (x.getLsbD 2) (x.getLsbD 1) (x.getLsbD 0) = x := by
  unfold packByte bitByte
  simp only [List.foldl_cons, List.foldl_nil]
  bv_decide

#print axioms byte_bits_pack
#print axioms pack_byte_bits

-- At finish, the accumulator contains n written bits in its low positions.
-- Shifting it left by 8 - n puts those bits first and zeroes the unused tail.
theorem partial_byte_bits (x : BitVec 8) (n : Nat) (hn : n ≤ 8) :
    (byteBits (x <<< (8 - n))).take n = (byteBits x).drop (8 - n) := by
  have casesN : n = 0 ∨ n = 1 ∨ n = 2 ∨ n = 3 ∨ n = 4 ∨
      n = 5 ∨ n = 6 ∨ n = 7 ∨ n = 8 := by omega
  rcases casesN with h | h | h | h | h | h | h | h | h <;>
    subst n <;> simp [byteBits] <;> bv_decide

theorem partial_byte_padding (x : BitVec 8) (n : Nat) (hn : n ≤ 8) :
    (byteBits (x <<< (8 - n))).drop n = List.replicate (8 - n) false := by
  have casesN : n = 0 ∨ n = 1 ∨ n = 2 ∨ n = 3 ∨ n = 4 ∨
      n = 5 ∨ n = 6 ∨ n = 7 ∨ n = 8 := by omega
  rcases casesN with h | h | h | h | h | h | h | h | h <;>
    subst n <;> simp [byteBits] <;> bv_decide

#print axioms partial_byte_bits
#print axioms partial_byte_padding

-- Reader abstraction after expanding bytes MSB-first. Exhausted reads return
-- false and set overrun, while each requested read still consumes one step.
def consumeBits : Nat → List Bool → List Bool × List Bool × Bool
  | 0, bits => ([], bits, false)
  | n + 1, bits =>
    let (bit, rest, missing) := match bits with
      | [] => (false, [], true)
      | b :: bs => (b, bs, false)
    let (read, remaining, overrun) := consumeBits n rest
    (bit :: read, remaining, missing || overrun)

theorem consume_prefix (xs suffix : List Bool) :
    consumeBits xs.length (xs ++ suffix) = (xs, suffix, false) := by
  induction xs with
  | nil => rfl
  | cons b bs ih => simp [consumeBits, ih]

theorem consume_exhausted : consumeBits 1 [] = ([false], [], true) := rfl

#print axioms consume_prefix
#print axioms consume_exhausted
