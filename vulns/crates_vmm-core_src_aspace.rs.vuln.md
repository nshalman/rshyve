# Vulnerability Report: `aspace.rs`

## Summary

Three bugs found in `InverseIter` and one in `covered_by`. The most exploitable are in `inverse_iter()`, which can report free memory regions outside the valid address space or wrap around the entire address space via integer overflow.

---

## Vulnerability 1: `inverse_iter` ignores address space start bound

**Severity: High**
**Type: Logic bug / out-of-bounds memory region reporting**
**Location: `inverse_iter()` (line ~133)**

### Root Cause

```rust
pub fn inverse_iter(&self) -> InverseIter<'_, T> {
    InverseIter { inner: self.map.iter(), next: 0, end: self.end }
    //                                          ^^^
    //                              Should be self.start, not 0
}
```

The iterator always starts scanning from address `0`, not `self.start`. When the address space starts at a non-zero address (e.g., `ASpace::new(0x1000, 0xFFFF)`), the iterator reports free regions **below the valid address space**.

### Exploitation

If the VMM uses `inverse_iter()` to find free memory for device placement or guest memory mapping, an attacker who controls the address space configuration could cause devices to be placed at addresses `[0, self.start)` — outside the intended bus region. In a PIO/MMIO context, this could map a device handler to a region that overlaps with a different bus or memory controller, leading to:

- Confused deputy: I/O directed to the wrong device handler
- Guest-accessible memory at unintended physical addresses

### PoC

```rust
let mut s: ASpace<()> = ASpace::new(0x1000, 0xFFFF);
s.register(0x2000, 0x1000, ()).unwrap();

let free: Vec<_> = s.inverse_iter().collect();
// Returns: Extent { start: 0x0, len: 0x2000 }  ← includes [0, 0xFFF] which is OUTSIDE the space
//          Extent { start: 0x3000, len: 0xCFFF }
```

All tests pass because they only test `ASpace::new(0, end)` where `self.start == 0`.

---

## Vulnerability 2: `InverseIter` integer overflow wraps to address 0

**Severity: High**
**Type: Integer overflow → address space wraparound**
**Location: `InverseIter::next()` (lines ~154, ~158)**

### Root Cause

```rust
self.next = *registered_start + registered_len;
//          ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
//          Overflows when region reaches end of usize address space
```

`safe_end()` validates that the *inclusive* end (`start + len - 1`) doesn't overflow, but the *exclusive* end (`start + len`) used here **can overflow to 0** when a region extends to `usize::MAX`.

### Exploitation

If the ASpace covers the full address range (`ASpace::new(0, usize::MAX)`) and a region is registered near the top:

```rust
let mut s: ASpace<()> = ASpace::new(0, usize::MAX);
// Register region covering top of address space
s.register(usize::MAX - 0xFFF, 0x1000, ()).unwrap();
// safe_end = (usize::MAX - 0xFFF - 1).checked_add(0x1000) = Some(usize::MAX) ✓

let free: Vec<_> = s.inverse_iter().collect();
// When processing this region:
//   self.next = (usize::MAX - 0xFFF) + 0x1000
//             = usize::MAX + 1
//             = 0  (overflow!)
//
// Iterator wraps to 0 and re-scans the entire address space,
// producing DUPLICATE free extents for already-reported regions
```

In a VMM, this could cause double-mapping of memory regions — two devices assigned the same address range.

---

## Vulnerability 3: `inverse_iter` off-by-one at end bound

**Severity: Medium**
**Type: Off-by-one**
**Location: `InverseIter::next()` loop condition + final extent calculation**

### Root Cause

`ASpace` uses **inclusive** bounds (`[self.start, self.end]`), but the iterator uses `while self.next < self.end` (strict less-than) and computes `self.end - self.next` (not `self.end - self.next + 1`). The address `self.end` is never reported as free space even when unregistered.

```rust
// ASpace::new(0, 100) = address space [0, 100] inclusive = 101 addresses
// register(0, 100) covers addresses 0-99 (safe_end(0,100) = 99)
// Address 100 is valid and unregistered, but inverse_iter() misses it

let mut s: ASpace<()> = ASpace::new(0, 100);
s.register(0, 100, ()).unwrap();  // covers 0..=99
let free: Vec<_> = s.inverse_iter().collect();
// Returns: [] (empty)
// Expected: [Extent { start: 100, len: 1 }]
```

The existing test `inverse_iterator_empty` claims "entire address space occupied" but it's wrong — address 100 is free.

---

## Vulnerability 4: `covered_by` integer overflow in Excluded branch

**Severity: Medium**
**Type: Integer overflow → incorrect range query**
**Location: `covered_by()`, `Excluded(pos)` arm**

### Root Cause

```rust
Excluded(pos) => {
    if let Ok((start, _, _)) = self.region_at(pos + 1) {
    //                                        ^^^^^^^
    //                          Wraps to 0 when pos = usize::MAX
```

In release mode, `usize::MAX + 1` wraps to `0`. This causes `region_at(0)` to be called instead of the intended query, returning a completely wrong region and corrupting the front bound of the range iterator.

Not directly triggered by `register()` (which uses `Included` bounds), but `covered_by` is a public method. If other VMM code calls `covered_by(Excluded(usize::MAX), ...)` to check for regions near the top of the address space, the overflow causes it to search from address 0 instead — potentially missing actual conflicts.

---

## Fix Sketch

```rust
pub fn inverse_iter(&self) -> InverseIter<'_, T> {
    InverseIter { inner: self.map.iter(), next: self.start, end: self.end }
}

// In InverseIter::next():
// Change loop: while self.next <= self.end
// Change advance: self.next = registered_start.checked_add(*registered_len).unwrap_or(self.end + 1)
// Change final extent len: self.end - self.next + 1

// In covered_by Excluded branch:
// Use pos.checked_add(1) and handle None
```
