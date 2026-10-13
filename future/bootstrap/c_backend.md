# C Backend

## Status: ⬜ Not started

## Why C?
C is the universal bootstrap target: every platform has a C compiler, so a C
backend avoids the chicken-and-egg problem of bootstrapping a systems
language.

## Translation scheme
| zr construct     | C equivalent                |
|------------------|----------------------------|
| `fn foo(x: i32)` | `int32_t foo(int32_t x)`   |
| `struct S { .. }`| `struct S { .. };`         |
| `&x`             | `&(x)`                     |
| `*p`             | `*(p)`                     |
| `if/else`        | `if (..) {..} else {..}`   |
| `while`          | `while (..) {..}`          |

## Tasks
- [ ] Emit a single C translation unit.
- [ ] Preserve symbol names (mangle later).
- [ ] Insert `#include <stdint.h>` and `<stddef.h>` headers.
- [ ] Forward-declare all functions to allow out-of-order definitions.
