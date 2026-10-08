# Unpacked Products

## Goal

Lay products out with natural alignment, as LLVM's ordinary (non-packed)
structs do, instead of as packed structs. Field order is unchanged; only
padding is added.

Packing was inherited from the first version of the compiler without a
deliberate decision, and it now costs more than it saves:

- **GC scanning.** A packed product can hold a pointer at any byte offset, so
  the collector checks a possible pointer at every byte of every scanned
  region (`__staple_gc_scan_region` in `gc.ll`: "Advancing one byte at a time
  catches pointers stored at arbitrary alignment"). With aligned pointers it
  can step one word at a time, about an eighth of the work.
- **Unaligned access.** 17 code generation sites use alignment-1 loads and
  stores. They are correct but slower on some targets, and they rule out
  atomic and vector operations later.
- **C interop.** C structs are padded. The coroutine runtime already converts
  its returned C struct into a packed product
  (`codegen/lowered/coroutines.rs`, "The runtime returns a C struct; Staple
  products are packed"), and any struct-passing FFI would need the same.

`Staple.md` leaves layouts unspecified ("foreign type layouts remain
unspecified"), so this is not a language change.

**Not a goal:** removing run-time offsets from dynamic code. With unpacked
products, a field after a run-time-typed one still has an offset that depends
on that type, now through both its size and its alignment. The
descriptor-taking converters discussed for the Stage 4 mixed-argument
limitation are needed either way; they should be written after U1, against
the aligned formula.

**Cost:** padding makes some values larger, for example `(I8, I64)` grows
from 9 to 16 bytes. U0 measures it. Reordering fields to reduce padding is out
of scope.

## Current state

U0–U2 are complete; U3–U5 remain pending. Concrete and dynamic products use
natural alignment. GC payload pointer words are aligned, buffers own their
descriptors, and headers preserve scalar alignment. Scanning remains byte-by-byte.

- Padding: **3,410 / 8,858** product-containing type occurrences grow;
  total store size rises **128,137 → 158,544 bytes (+23.73%)**.
- Packed GC benchmark: **0.524 s** median of five execution-only runs on
  this arm64 macOS host; U3 will repeat it.
- Workspace, layout sweep, and full differential gates pass. Behavior matches
  **119 / 119** pre-U1 outputs (`scripts/unpacked-products-reference.json`).

## The IR baseline

Every stage so far has been gated on default-mode IR matching a release
build of `50565d8`. U1 changes the IR of every program that contains a
product, so that comparison cannot gate this work. Instead:

- **Behavior gate.** Every runnable corpus program and example must print the
  same stdout and exit with the same status as the release build of the
  commit before U1. A small script (`scripts/compare-run-output.py`) builds
  both and compares them.
- **Layout gate.** The two layout tests above, which must still pass, now
  against the aligned layout.
- **Re-baseline.** After U4, a release build of the final commit becomes the
  new IR reference for later stages, recorded in the Stage 4 breakdown.

## Steps

Each step ends with `cargo test --workspace --quiet` passing, the full
differential run passing, and the behavior gate matching.

### U0 — Measure (complete)

- Record, across the corpus, the total store size of every concrete type
  that is a product or contains one, and the number of types whose size
  changes. Repeat after U1 to report the padding cost.
- Time a GC-heavy corpus program, or a small dedicated benchmark that
  allocates many products holding pointers, as the baseline for U3.
- Write `scripts/compare-run-output.py` and capture the reference outputs.

### U1 — Aligned products (complete)

Concrete and dynamic layouts must change together: the bit-compatibility of
values passing between concrete and dynamic code depends on their agreement.

- `compile_product_type` and the product builders in `ir.rs` build
  non-packed structs. The empty product stays an empty struct.
- `DescriptorLayout::packed_product` becomes `product`: each field starts at
  the previous end rounded up to its alignment, the alignment is the largest
  field alignment (at least 1), and store size and stride are the end rounded
  up to that alignment, matching LLVM's struct layout. Sums follow through
  `DescriptorLayout::sum`.
- `runtime_product_layout`, `dynamic_product_field`, and the run-time product
  descriptor builders compute the same offsets from descriptors at run time,
  with the existing `runtime_align_up`.
- `conversion_field_pointer` uses the aligned offsets.
- The coroutine runtime conversion becomes a direct use of the C struct
  where layouts now agree, or keeps an explicit conversion where they don't.
- Update JIT tests that hard-code packed offsets.
- **Audit**: every use of `store_size` as an offset, and every
  `struct_type(…, true)`, is either changed or recorded with the reason it
  stays. The audit list goes in the commit message.

### U2 — Aligned GC payloads (complete)

Before the GC can scan by word, every pointer it might need to find must be
word-aligned.

- Captured cells: the descriptor word moves to
  `align_up(store_size + 1, pointer alignment)`, and the cell finalizer and
  every reader use the same offset.
- Buffers: the header offset and the trailing descriptor word use aligned
  offsets. Element alignment is now real for product elements.
- Managed references (`Ref` payloads with a trailing descriptor word) and any
  other trailing-descriptor allocation: aligned like cells.
- Check that `__staple_gc_alloc` returns payloads aligned to at least the
  pointer size, and record the guarantee in `gc.ll`.
- Audit every `build_gc_allocation` site and both runtime `.ll` modules for
  stored pointers at unaligned offsets.
- Add a test per payload kind that asserts its pointer-holding words are
  pointer-aligned.

### U3 — Word-by-word GC scanning

- `__staple_gc_scan_region` rounds the region's start up to pointer
  alignment and advances by the pointer size.
- Explicit root regions registered with sizes smaller than a pointer (for
  example 1-byte state cells) hold no pointers; the scanner skips a region
  shorter than one aligned word.
- Re-time the U0 benchmark and record the result.

### U4 — Natural alignment for known accesses

- Replace alignment-1 loads and stores with the type's alignment wherever the
  address is known to be aligned: concrete storage, and dynamic storage
  addressed through aligned offsets. Keep alignment 1 where the address can
  still be unaligned, with a comment saying why.

### U5 — Documentation and re-baseline

- Update `HYBRID_POLYMORPHISM_PLAN.md` ("Current State", Layout) and
  `STAGE_4_DICTIONARY_PASSING_BREAKDOWN.md` (Decision 5) to describe the
  aligned layout.
- Build the release reference binary at the final commit and record it as the
  IR baseline for later stages.

## Sequencing

Do this before lifting the Stage 4 mixed-argument limitation with
descriptor-taking converters, and before Stage 6, so that neither is written
against the packed layout and then rewritten.

## Risks

- **Concrete/dynamic disagreement.** A product laid out differently by LLVM
  and by descriptors corrupts every value crossing between concrete and
  dynamic code. The layout sweep and the full differential run are the
  guards; U1 changes both sides in one step for this reason.
- **Hidden packed assumptions.** Offsets computed as plain sums of store sizes
  outside the places listed above would silently break. U1's audit lists
  them all.
- **Missed pointers after U3.** If any pointer is still stored at an
  unaligned offset when scanning becomes word-aligned, the collector misses
  it and frees live memory. U2's alignment tests and audit must be complete
  before U3, and U3 lands as its own commit so it can be reverted alone.
- **Memory growth.** Padding increases some values' size. U0 and U1 measure
  it; if it is significant, field reordering can be considered separately.
