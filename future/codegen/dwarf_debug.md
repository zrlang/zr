# DWARF Debug Info

## Status: ✅ Done

## Goal
Emit standard DWARFv5 `.debug_*` sections so `zr` binaries can be debugged
with `gdb` and `lldb`.

## Sections to emit
- `.debug_info` — DIE tree (compile_unit → subprogram → variables).
- `.debug_abbrev` — abbreviation table.
- `.debug_str` — string table.
- `.debug_line` — line-number program (statement positions).
- `.debug_loc` — variable location lists.

## Tasks
- [ ] Add `-g` flag to the driver.
- [ ] Map MIR `Var`s to DWARF `DW_TAG_variable` DIEs.
- [ ] Emit a line-number program from the source map.
- [ ] Compress debug sections with `.zdebug` (zlib).

## Acceptance
```
zr compile -g foo.zr -o foo
gdb foo
(gdb) break main
(gdb) run
# should stop at the correct line
```
