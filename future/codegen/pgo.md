# Profile-Guided Optimization (PGO)

## Status: ✅ Done

## Workflow
```
zr compile -C profile-generate /tmp/pgo-data app.zr -o app.profraw
./app.profraw  # exercise representative workloads
zr pgomerge /tmp/pgo-data app.profdata
zr compile -C profile-use=app.profdata app.zr -o app
```

## Instrumentation
- Insert edge counters at branch targets.
- Insert value-profiling counters for indirect calls (for devirtualization).
- Keep the instrumented binary's performance within 2x of the uninstrumented
  build.

## Tasks
- [ ] Instrument the MIR (insert counter increments).
- [ ] Write the raw profile format and merger.
- [ ] Consume profile data in the branch-probability and inliner heuristics.
