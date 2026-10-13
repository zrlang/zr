# Waker / Context Integration

## Status: ✅ Done

## Problem
`.await` needs a way to register interest in readiness and be notified when an
I/O event occurs. This requires `Waker`, `Context`, and the `Wake` trait.

## Tasks
- [ ] Define `trait Wake { fn wake(self: Box<Self>); fn wake_by_ref(&self); }`.
- [ ] Implement `Waker` as an `Arc<dyn Wake>` wrapper (one-arc, one-vtable-ptr).
- [ ] Define `Context { waker: &Waker }` and pass it through `poll`.
- [ ] In async lowering, generate `poll(self: Pin<&mut Self>, cx: &mut Context)`.
- [ ] Lower `.await` to: store `cx.waker()`, call sub-future's `poll`, yield
      `Poll::Pending` when the sub-future returns `Pending`.

## Acceptance
A single-threaded executor that drives a timer future to completion must print
`done` exactly once.
