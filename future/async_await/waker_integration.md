# Waker / Context Integration

## Status: 🔧 Partial（2026-11-05 核对）

`trait Wake`（`wake(self: Box<Self>)` / `wake_by_ref(self: &Self)`）、
`Waker`（`new` / `wake` 消费 / `wake_by_ref` 借用）、`Context`（`new` /
`waker`）都在 `std/alloc/async.zr` 实现；`Box<dyn Wake>` 的 vtable 分派
（`Box<Self>` 与 `&Self` 两种接收者、`*mut T as *mut dyn Tr` 指针 fatten、
`Box::into_raw` / `Box::from_raw` 重建）已经 `169_box_dyn.zr` 验证可用。
仍缺：async 降级生成的 `poll` 签名没有 `cx: &mut Context`——`.await` 的
Context 传递、timer 驱动的 readiness 通知都未接线。

## Problem
`.await` needs a way to register interest in readiness and be notified when an
I/O event occurs. This requires `Waker`, `Context`, and the `Wake` trait.

## Tasks
- [x] Define `trait Wake { fn wake(self: Box<Self>); fn wake_by_ref(&self); }`.
- [x] Implement `Waker` as an `Arc<dyn Wake>` wrapper (one-arc, one-vtable-ptr).
- [x] Define `Context { waker: &Waker }` and pass it through `poll`.
- [ ] In async lowering, generate `poll(self: Pin<&mut Self>, cx: &mut Context)`.
- [ ] Lower `.await` to: store `cx.waker()`, call sub-future's `poll`, yield
      `Poll::Pending` when the sub-future returns `Pending`.

## Acceptance
A single-threaded executor that drives a timer future to completion must print
`done` exactly once.
