; Coroutine trampoline driver and scheduler runtime.
;
; A coroutine `resume` function has the signature `%CoroStatus (ptr frame)` and
; returns:
;   0  DONE          - the body ran to its end; the result is at frame->result_ptr
;   1  RESUME_CHILD   - the body awaits a child coroutine (the {i8,ptr} carries it)
;   2  SUSPENDED      - the body parked itself on the scheduler (`yield_now`)
;   3  CANCELLED      - the body unwound after a cancellation request (Step 3d)
;   4  WAIT_TASK      - the body awaits a spawned `Task`; it has registered itself
;                       as that task's waiter and is woken on the task's completion
;
; `__staple_coro_drive` itself returns 0 (whole chain completed), 2 (a frame
; parked via `yield_now`; the caller re-queues the parked leaf) or 3 (a frame is
; waiting on a task; the caller must NOT re-queue it — the awaited task wakes it).
;
; The driver threads the parent chain through frame->parent, so an arbitrarily
; deep `await` nest costs no native stack.

%CoroStatus = type { i8, ptr }
; The header prefix every coroutine frame starts with:
;   { i8 state, ptr resume_fn, ptr cleanup_fn, ptr capture_env,
;     ptr child, ptr result_ptr, ptr pending_ptr, ptr parent,
;     ptr resources, ptr record }
%CoroHeader = type { i8, ptr, ptr, ptr, ptr, ptr, ptr, ptr, ptr, ptr }

; { ptr scheduler, ptr tasks_head } — tasks_head chains the scope's live task
; records (most-recent first) through %TaskRecord.scope_next for teardown.
%TaskScope = type { ptr, ptr }

; The header prefix of a `spawn`ed task's result record. `spawn` allocates
; `{ i8 state, i8 cancel, ptr frame, ptr waiter, ptr scheduler, ptr scope_next,
; T result }`; the runtime only ever touches the header. state: 0 pending,
; 1 completed, 2 cancelled. cancel: nonzero once cancellation is requested.
%TaskRecord = type { i8, i8, ptr, ptr, ptr, ptr }

; { ptr ready_head, ptr ready_tail, i8 pumping }
%Scheduler = type { ptr, ptr, i8 }

; A ready-queue node: { ptr frame, ptr next }
%CoroNode = type { ptr, ptr }

; The header of a `completion` record, shared by its `Wait` and `Resolver`
; handles. `completion` allocates `{ <this header>, T value }`. state: 0 pending,
; 1 completed, 2 cancelled (resolver), 3 consumer-gone. flags bit0: a waiter is
; registered. generation: bumped on register — reserved for slice 4b.
%Completion = type { i8, i8, {{SIZE}}, ptr, ptr, ptr, ptr }

declare void @llvm.trap()
declare ptr @malloc({{SIZE}})
declare void @free(ptr)
declare void @llvm.memcpy.p0.p0.{{SIZE}}(ptr, ptr, {{SIZE}}, i1)
declare void @__staple_gc_unregister_root(ptr)
; Reactive runtime (linked in first): tracking is suspended around every `resume`
; so a signal read inside a coroutine body never subscribes an enclosing
; reaction, and `pump` refuses to run while a reaction or batch is active.
declare ptr @__staple_tracking_suspend()
declare void @__staple_tracking_restore(ptr)
declare i1 @__staple_reactive_guard_active()
declare ptr @__staple_reaction_create(ptr, ptr, ptr, {{SIZE}})
declare void @__staple_reaction_clear(ptr)
declare void @__staple_gc_register_root(ptr, {{SIZE}})

; ---------------------------------------------------------------------------
; Trampoline

; Drive from `%start` (a task root, or a parked leaf) until the whole chain
; completes (returns 0) or a frame suspends (returns 2 and stores the parked
; leaf into `%leaf_out`). Does not touch `%start->parent` — the caller owns the
; parent linkage.
define i8 @__staple_coro_drive(ptr %start, ptr %leaf_out) {
entry:
  br label %loop

loop:
  %top = phi ptr [ %start, %entry ], [ %parent, %pop ], [ %child.ptr, %push ]
  %resume.slot = getelementptr inbounds %CoroHeader, ptr %top, i32 0, i32 1
  %resume.fn = load ptr, ptr %resume.slot
  ; Detach reactive dependency tracking for the duration of the body: a signal
  ; read in a coroutine must not attach to whatever reaction was current.
  %prev.tracking = call ptr @__staple_tracking_suspend()
  %r = call %CoroStatus %resume.fn(ptr %top)
  call void @__staple_tracking_restore(ptr %prev.tracking)
  %status = extractvalue %CoroStatus %r, 0
  switch i8 %status, label %bad [
    i8 0, label %done
    i8 1, label %push
    i8 2, label %suspended
    i8 3, label %done
    i8 4, label %wait_task
  ]

done:
  %parent.slot = getelementptr inbounds %CoroHeader, ptr %top, i32 0, i32 7
  %parent = load ptr, ptr %parent.slot
  %at.root = icmp eq ptr %parent, null
  br i1 %at.root, label %finish.root, label %cleanup.child

finish.root:
  ; Mark the task record complete, if this frame carries one.
  %record.slot = getelementptr inbounds %CoroHeader, ptr %top, i32 0, i32 9
  %record = load ptr, ptr %record.slot
  %has.record = icmp ne ptr %record, null
  br i1 %has.record, label %mark.record, label %cleanup.root

mark.record:
  ; Publish the task's terminal state once: 1 completed, 2 cancelled (status 3).
  ; A redundant re-drive of an already-terminal record leaves it untouched.
  %rec.state = load i8, ptr %record
  %rec.pending = icmp eq i8 %rec.state, 0
  br i1 %rec.pending, label %set.record, label %after.set

set.record:
  %was.cancel = icmp eq i8 %status, 3
  %terminal = select i1 %was.cancel, i8 2, i8 1
  store i8 %terminal, ptr %record
  br label %after.set

after.set:
  ; If a coroutine is `await`-ing this task, re-queue it on the task's own
  ; scheduler so the next pump resumes it past the `await`.
  %rec.waiter.slot = getelementptr inbounds %TaskRecord, ptr %record, i32 0, i32 3
  %rec.waiter = load ptr, ptr %rec.waiter.slot
  %has.waiter = icmp ne ptr %rec.waiter, null
  br i1 %has.waiter, label %wake.waiter, label %cleanup.root

wake.waiter:
  ; Skip a waiter frame that has itself already run out or been freed.
  %waiter.state = load i8, ptr %rec.waiter
  %waiter.live = icmp ult i8 %waiter.state, -2
  br i1 %waiter.live, label %do.wake, label %cleanup.root

do.wake:
  %rec.sched.slot = getelementptr inbounds %TaskRecord, ptr %record, i32 0, i32 4
  %rec.sched = load ptr, ptr %rec.sched.slot
  store ptr null, ptr %rec.waiter.slot
  call void @__staple_sched_enqueue(ptr %rec.sched, ptr %rec.waiter)
  br label %cleanup.root

cleanup.root:
  %cleanup.root.slot = getelementptr inbounds %CoroHeader, ptr %top, i32 0, i32 2
  %cleanup.root.fn = load ptr, ptr %cleanup.root.slot
  call void %cleanup.root.fn(ptr %top)
  ret i8 0

cleanup.child:
  %cleanup.slot = getelementptr inbounds %CoroHeader, ptr %top, i32 0, i32 2
  %cleanup.fn = load ptr, ptr %cleanup.slot
  call void %cleanup.fn(ptr %top)
  br label %pop

pop:
  %parent.child = getelementptr inbounds %CoroHeader, ptr %parent, i32 0, i32 4
  store ptr null, ptr %parent.child
  br label %loop

push:
  %child.ptr = extractvalue %CoroStatus %r, 1
  %child.parent = getelementptr inbounds %CoroHeader, ptr %child.ptr, i32 0, i32 7
  store ptr %top, ptr %child.parent
  %top.pending = getelementptr inbounds %CoroHeader, ptr %top, i32 0, i32 6
  %pending = load ptr, ptr %top.pending
  %child.result = getelementptr inbounds %CoroHeader, ptr %child.ptr, i32 0, i32 5
  store ptr %pending, ptr %child.result
  br label %loop

suspended:
  store ptr %top, ptr %leaf_out
  ret i8 2

wait_task:
  ; The frame has already registered itself as the awaited task's waiter; unwind
  ; without re-queueing it. Return 3 so `pump` leaves it off the carry list.
  store ptr %top, ptr %leaf_out
  ret i8 3

bad:
  call void @llvm.trap()
  unreachable
}

; ---------------------------------------------------------------------------
; `yield_now` coroutine: suspends once, then completes with `()`.

define %CoroStatus @__staple_coro_yield_resume(ptr %frame) {
entry:
  %state = load i8, ptr %frame
  %first = icmp eq i8 %state, 0
  br i1 %first, label %yield, label %done

yield:
  store i8 1, ptr %frame
  ret %CoroStatus { i8 2, ptr null }

done:
  store i8 -2, ptr %frame            ; 254 = DONE marker
  ret %CoroStatus { i8 0, ptr null }
}

define void @__staple_coro_yield_cleanup(ptr %frame) {
entry:
  %state = load i8, ptr %frame
  %freed = icmp eq i8 %state, -1     ; 255 = FREED marker
  br i1 %freed, label %done, label %free

free:
  call void @__staple_gc_unregister_root(ptr %frame)
  store i8 -1, ptr %frame
  br label %done

done:
  ret void
}

; ---------------------------------------------------------------------------
; Scheduler

define ptr @__staple_sched_create() {
entry:
  %sched = call ptr @malloc({{SIZE}} ptrtoint (ptr getelementptr (%Scheduler, ptr null, i32 1) to {{SIZE}}))
  %head = getelementptr inbounds %Scheduler, ptr %sched, i32 0, i32 0
  store ptr null, ptr %head
  %tail = getelementptr inbounds %Scheduler, ptr %sched, i32 0, i32 1
  store ptr null, ptr %tail
  %pumping = getelementptr inbounds %Scheduler, ptr %sched, i32 0, i32 2
  store i8 0, ptr %pumping
  ret ptr %sched
}

define void @__staple_sched_destroy(ptr %sched) {
entry:
  %head.slot = getelementptr inbounds %Scheduler, ptr %sched, i32 0, i32 0
  br label %loop
loop:
  %node = load ptr, ptr %head.slot
  %empty = icmp eq ptr %node, null
  br i1 %empty, label %done, label %drop
drop:
  %next.slot = getelementptr inbounds %CoroNode, ptr %node, i32 0, i32 1
  %next = load ptr, ptr %next.slot
  store ptr %next, ptr %head.slot
  call void @free(ptr %node)
  br label %loop
done:
  call void @free(ptr %sched)
  ret void
}

define void @__staple_sched_enqueue(ptr %sched, ptr %frame) {
entry:
  %node = call ptr @malloc({{SIZE}} ptrtoint (ptr getelementptr (%CoroNode, ptr null, i32 1) to {{SIZE}}))
  %node.frame = getelementptr inbounds %CoroNode, ptr %node, i32 0, i32 0
  store ptr %frame, ptr %node.frame
  %node.next = getelementptr inbounds %CoroNode, ptr %node, i32 0, i32 1
  store ptr null, ptr %node.next
  %tail.slot = getelementptr inbounds %Scheduler, ptr %sched, i32 0, i32 1
  %tail = load ptr, ptr %tail.slot
  %empty = icmp eq ptr %tail, null
  br i1 %empty, label %first, label %append
first:
  %head.slot = getelementptr inbounds %Scheduler, ptr %sched, i32 0, i32 0
  store ptr %node, ptr %head.slot
  store ptr %node, ptr %tail.slot
  ret void
append:
  %tail.next = getelementptr inbounds %CoroNode, ptr %tail, i32 0, i32 1
  store ptr %node, ptr %tail.next
  store ptr %node, ptr %tail.slot
  ret void
}

; Run a bounded snapshot of the ready queue.
;   - snapshot = the ready queue at entry; the queue is emptied
;   - each snapshot frame is driven once (up to `%limit`)
;   - frames that suspend are collected into `carry`
;   - frames spawned during the pump land back on the (now-refilling) ready queue
;   - for the next pump: undriven snapshot, then carry, then the spawns
; Returns { executed, still-ready }.
define { {{SIZE}}, {{SIZE}} } @__staple_sched_pump(ptr %sched, {{SIZE}} %limit) {
entry:
  ; A pump must not run inside a reaction (a reaction may never enter the driver)
  ; or an open batch (it would run effects the batch defers).
  %reactive.active = call i1 @__staple_reactive_guard_active()
  br i1 %reactive.active, label %bad, label %check.reentry
check.reentry:
  %pumping.slot = getelementptr inbounds %Scheduler, ptr %sched, i32 0, i32 2
  %pumping = load i8, ptr %pumping.slot
  %reentrant = icmp ne i8 %pumping, 0
  br i1 %reentrant, label %bad, label %start
bad:
  call void @llvm.trap()
  unreachable
start:
  store i8 1, ptr %pumping.slot
  %head.slot = getelementptr inbounds %Scheduler, ptr %sched, i32 0, i32 0
  %tail.slot = getelementptr inbounds %Scheduler, ptr %sched, i32 0, i32 1
  %snapshot0 = load ptr, ptr %head.slot
  store ptr null, ptr %head.slot
  store ptr null, ptr %tail.slot

  %leaf.out = alloca ptr
  %snapshot = alloca ptr
  %carry.head = alloca ptr
  %carry.tail = alloca ptr
  %executed = alloca {{SIZE}}
  store ptr %snapshot0, ptr %snapshot
  store ptr null, ptr %carry.head
  store ptr null, ptr %carry.tail
  store {{SIZE}} 0, ptr %executed
  br label %loop

loop:
  %n = load ptr, ptr %snapshot
  %at.end = icmp eq ptr %n, null
  br i1 %at.end, label %splice, label %check.limit
check.limit:
  %count = load {{SIZE}}, ptr %executed
  %hit = icmp uge {{SIZE}} %count, %limit
  br i1 %hit, label %splice, label %run
run:
  %frame.slot = getelementptr inbounds %CoroNode, ptr %n, i32 0, i32 0
  %frame = load ptr, ptr %frame.slot
  %next.slot = getelementptr inbounds %CoroNode, ptr %n, i32 0, i32 1
  %next = load ptr, ptr %next.slot
  store ptr %next, ptr %snapshot
  call void @free(ptr %n)
  %status = call i8 @__staple_coro_drive(ptr %frame, ptr %leaf.out)
  %count2 = load {{SIZE}}, ptr %executed
  %count3 = add {{SIZE}} %count2, 1
  store {{SIZE}} %count3, ptr %executed
  %suspended = icmp eq i8 %status, 2
  br i1 %suspended, label %park, label %loop
park:
  %leaf = load ptr, ptr %leaf.out
  %pnode = call ptr @malloc({{SIZE}} ptrtoint (ptr getelementptr (%CoroNode, ptr null, i32 1) to {{SIZE}}))
  %pnode.frame = getelementptr inbounds %CoroNode, ptr %pnode, i32 0, i32 0
  store ptr %leaf, ptr %pnode.frame
  %pnode.next = getelementptr inbounds %CoroNode, ptr %pnode, i32 0, i32 1
  store ptr null, ptr %pnode.next
  %ch = load ptr, ptr %carry.head
  %carry.empty = icmp eq ptr %ch, null
  br i1 %carry.empty, label %carry.first, label %carry.append
carry.first:
  store ptr %pnode, ptr %carry.head
  store ptr %pnode, ptr %carry.tail
  br label %loop
carry.append:
  %ct = load ptr, ptr %carry.tail
  %ct.next = getelementptr inbounds %CoroNode, ptr %ct, i32 0, i32 1
  store ptr %pnode, ptr %ct.next
  store ptr %pnode, ptr %carry.tail
  br label %loop

splice:
  ; Next ready = undriven-snapshot ++ carry ++ current-ready(spawns).
  %spawns = load ptr, ptr %head.slot
  %rem = load ptr, ptr %snapshot
  %ch2 = load ptr, ptr %carry.head
  %ct2 = load ptr, ptr %carry.tail
  ; carry ++ spawns
  %carry.plus = call { ptr, ptr } @__staple_coro_node_concat(ptr %ch2, ptr %ct2, ptr %spawns, ptr null)
  %cp.head = extractvalue { ptr, ptr } %carry.plus, 0
  %cp.tail = extractvalue { ptr, ptr } %carry.plus, 1
  ; rem ++ (carry ++ spawns)
  %rem.tail = call ptr @__staple_coro_node_tail(ptr %rem)
  %final = call { ptr, ptr } @__staple_coro_node_concat(ptr %rem, ptr %rem.tail, ptr %cp.head, ptr %cp.tail)
  %f.head = extractvalue { ptr, ptr } %final, 0
  %f.tail = extractvalue { ptr, ptr } %final, 1
  store ptr %f.head, ptr %head.slot
  store ptr %f.tail, ptr %tail.slot
  store i8 0, ptr %pumping.slot

  %ready = call {{SIZE}} @__staple_coro_node_len(ptr %f.head)
  %done.count = load {{SIZE}}, ptr %executed
  %r0 = insertvalue { {{SIZE}}, {{SIZE}} } undef, {{SIZE}} %done.count, 0
  %r1 = insertvalue { {{SIZE}}, {{SIZE}} } %r0, {{SIZE}} %ready, 1
  ret { {{SIZE}}, {{SIZE}} } %r1
}

; Concatenate two node lists. `a_tail`/`b_tail` may be null, in which case the
; tail is recomputed. Returns { head, tail }.
define { ptr, ptr } @__staple_coro_node_concat(ptr %a.head, ptr %a.head.tail, ptr %b.head, ptr %b.head.tail) {
entry:
  %a.empty = icmp eq ptr %a.head, null
  br i1 %a.empty, label %use.b, label %join
use.b:
  %bt.given = icmp ne ptr %b.head.tail, null
  br i1 %bt.given, label %b.ret, label %b.compute
b.compute:
  %bt = call ptr @__staple_coro_node_tail(ptr %b.head)
  br label %b.ret
b.ret:
  %b.tail.final = phi ptr [ %b.head.tail, %use.b ], [ %bt, %b.compute ]
  %rb0 = insertvalue { ptr, ptr } undef, ptr %b.head, 0
  %rb1 = insertvalue { ptr, ptr } %rb0, ptr %b.tail.final, 1
  ret { ptr, ptr } %rb1
join:
  %at.given = icmp ne ptr %a.head.tail, null
  br i1 %at.given, label %have.at, label %compute.at
compute.at:
  %at.c = call ptr @__staple_coro_node_tail(ptr %a.head)
  br label %have.at
have.at:
  %at = phi ptr [ %a.head.tail, %join ], [ %at.c, %compute.at ]
  %at.next = getelementptr inbounds %CoroNode, ptr %at, i32 0, i32 1
  store ptr %b.head, ptr %at.next
  %b.empty = icmp eq ptr %b.head, null
  br i1 %b.empty, label %tail.is.a, label %tail.is.b
tail.is.a:
  %ra0 = insertvalue { ptr, ptr } undef, ptr %a.head, 0
  %ra1 = insertvalue { ptr, ptr } %ra0, ptr %at, 1
  ret { ptr, ptr } %ra1
tail.is.b:
  %bt2.given = icmp ne ptr %b.head.tail, null
  br i1 %bt2.given, label %j.ret, label %j.compute
j.compute:
  %bt2 = call ptr @__staple_coro_node_tail(ptr %b.head)
  br label %j.ret
j.ret:
  %b.tail2 = phi ptr [ %b.head.tail, %tail.is.b ], [ %bt2, %j.compute ]
  %rj0 = insertvalue { ptr, ptr } undef, ptr %a.head, 0
  %rj1 = insertvalue { ptr, ptr } %rj0, ptr %b.tail2, 1
  ret { ptr, ptr } %rj1
}

define ptr @__staple_coro_node_tail(ptr %head) {
entry:
  %empty = icmp eq ptr %head, null
  br i1 %empty, label %none, label %walk
none:
  ret ptr null
walk:
  %cur = phi ptr [ %head, %entry ], [ %next, %advance ]
  %next.slot = getelementptr inbounds %CoroNode, ptr %cur, i32 0, i32 1
  %next = load ptr, ptr %next.slot
  %is.tail = icmp eq ptr %next, null
  br i1 %is.tail, label %found, label %advance
advance:
  br label %walk
found:
  ret ptr %cur
}

define {{SIZE}} @__staple_coro_node_len(ptr %head) {
entry:
  br label %loop
loop:
  %cur = phi ptr [ %head, %entry ], [ %next, %step ]
  %count = phi {{SIZE}} [ 0, %entry ], [ %count1, %step ]
  %empty = icmp eq ptr %cur, null
  br i1 %empty, label %done, label %step
step:
  %next.slot = getelementptr inbounds %CoroNode, ptr %cur, i32 0, i32 1
  %next = load ptr, ptr %next.slot
  %count1 = add {{SIZE}} %count, 1
  br label %loop
done:
  ret {{SIZE}} %count
}

; ---------------------------------------------------------------------------
; Task scopes

define ptr @__staple_task_scope_open(ptr %sched) {
entry:
  %scope = call ptr @malloc({{SIZE}} ptrtoint (ptr getelementptr (%TaskScope, ptr null, i32 1) to {{SIZE}}))
  %sched.slot = getelementptr inbounds %TaskScope, ptr %scope, i32 0, i32 0
  store ptr %sched, ptr %sched.slot
  %tasks.slot = getelementptr inbounds %TaskScope, ptr %scope, i32 0, i32 1
  store ptr null, ptr %tasks.slot
  ret ptr %scope
}

; Link a freshly-spawned task record into its scope's list (most-recent first).
define void @__staple_task_scope_track(ptr %scope, ptr %record) {
entry:
  %head.slot = getelementptr inbounds %TaskScope, ptr %scope, i32 0, i32 1
  %old = load ptr, ptr %head.slot
  %next.slot = getelementptr inbounds %TaskRecord, ptr %record, i32 0, i32 5
  store ptr %old, ptr %next.slot
  store ptr %record, ptr %head.slot
  ret void
}

; Close a scope: cancel every still-pending task, youngest first, and drive it
; to the end of its unwind so teardown is synchronous and ordered.
define void @__staple_task_scope_close(ptr %scope) {
entry:
  %head.slot = getelementptr inbounds %TaskScope, ptr %scope, i32 0, i32 1
  %first = load ptr, ptr %head.slot
  %leaf = alloca ptr
  br label %loop

loop:
  %rec = phi ptr [ %first, %entry ], [ %next, %advance ]
  %at.end = icmp eq ptr %rec, null
  br i1 %at.end, label %release, label %visit

visit:
  %next.slot = getelementptr inbounds %TaskRecord, ptr %rec, i32 0, i32 5
  %next = load ptr, ptr %next.slot
  %st = load i8, ptr %rec
  %pending = icmp eq i8 %st, 0
  br i1 %pending, label %cancel, label %advance

cancel:
  %cancel.slot = getelementptr inbounds %TaskRecord, ptr %rec, i32 0, i32 1
  store i8 1, ptr %cancel.slot
  %frame.slot = getelementptr inbounds %TaskRecord, ptr %rec, i32 0, i32 2
  %frame = load ptr, ptr %frame.slot
  %drv = call i8 @__staple_coro_drive(ptr %frame, ptr %leaf)
  br label %advance

advance:
  br label %loop

release:
  call void @free(ptr %scope)
  ret void
}

; Request cancellation of a task. Idempotent; a no-op once the task is finished.
; A pending task is re-queued so the next pump drives it into its unwind.
define void @__staple_task_cancel(ptr %record) {
entry:
  %state = load i8, ptr %record
  %finished = icmp ne i8 %state, 0
  br i1 %finished, label %done, label %check.flag

check.flag:
  %cancel.slot = getelementptr inbounds %TaskRecord, ptr %record, i32 0, i32 1
  %flag = load i8, ptr %cancel.slot
  %already = icmp ne i8 %flag, 0
  br i1 %already, label %done, label %request

request:
  store i8 1, ptr %cancel.slot
  %frame.slot = getelementptr inbounds %TaskRecord, ptr %record, i32 0, i32 2
  %frame = load ptr, ptr %frame.slot
  %sched.slot = getelementptr inbounds %TaskRecord, ptr %record, i32 0, i32 4
  %sched = load ptr, ptr %sched.slot
  call void @__staple_sched_enqueue(ptr %sched, ptr %frame)
  br label %done

done:
  ret void
}

; ---------------------------------------------------------------------------
; Completions (general wait interface)

; Register `%frame` as the sole waiter on a task's result record. Returns 1 if
; the caller should suspend, 0 if the task has already finished (the awaiting
; body reads the record and continues in the same resume).
define i8 @__staple_task_await_register(ptr %record, ptr %frame) {
entry:
  %state = load i8, ptr %record
  %resolved = icmp ne i8 %state, 0
  br i1 %resolved, label %ready, label %park
ready:
  ret i8 0
park:
  %waiter.slot = getelementptr inbounds %TaskRecord, ptr %record, i32 0, i32 3
  store ptr %frame, ptr %waiter.slot
  ret i8 1
}

; Register `%frame` as the waiter on a completion. Traps if `%sched` is a
; non-null scheduler other than the completion's own. Returns 1 to suspend, 0 if
; the completion is already resolved.
define i8 @__staple_completion_register(ptr %record, ptr %frame, ptr %sched) {
entry:
  %own.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 3
  %own = load ptr, ptr %own.slot
  %not.null = icmp ne ptr %sched, null
  %mismatch = icmp ne ptr %sched, %own
  %cross = and i1 %not.null, %mismatch
  br i1 %cross, label %bad, label %check
bad:
  call void @llvm.trap()
  unreachable
check:
  %state = load i8, ptr %record
  %resolved = icmp ne i8 %state, 0
  br i1 %resolved, label %ready, label %park
ready:
  ret i8 0
park:
  %waiter.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 4
  store ptr %frame, ptr %waiter.slot
  %gen.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 2
  %gen = load {{SIZE}}, ptr %gen.slot
  %gen1 = add {{SIZE}} %gen, 1
  store {{SIZE}} %gen1, ptr %gen.slot
  ; Set the "waiter registered" bit without disturbing the cancel-armed bit.
  %flags.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 1
  %flags = load i8, ptr %flags.slot
  %flags1 = or i8 %flags, 1
  store i8 %flags1, ptr %flags.slot
  ret i8 1
}

; Run the armed cancellation callback exactly once (consumer-side abandonment).
define internal void @__staple_completion_run_cancel(ptr %record) {
entry:
  %flags.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 1
  %flags = load i8, ptr %flags.slot
  %armed = and i8 %flags, 2
  %is.armed = icmp ne i8 %armed, 0
  br i1 %is.armed, label %invoke, label %done
invoke:
  ; Clear the armed bit before invoking so a re-entrant abandon cannot double-run.
  %cleared = and i8 %flags, -3
  store i8 %cleared, ptr %flags.slot
  %env.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 5
  %env = load ptr, ptr %env.slot
  %fn.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 6
  %fn = load ptr, ptr %fn.slot
  call void %fn(ptr %env)
  br label %done
done:
  ret void
}

; Release the armed cancellation callback without running it (successful
; resolution). The closure environment is GC-managed; dropping the reference is
; enough.
define internal void @__staple_completion_disarm_cancel(ptr %record) {
entry:
  %flags.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 1
  %flags = load i8, ptr %flags.slot
  %cleared = and i8 %flags, -3
  store i8 %cleared, ptr %flags.slot
  %env.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 5
  store ptr null, ptr %env.slot
  %fn.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 6
  store ptr null, ptr %fn.slot
  ret void
}

; Wake a completion's registered waiter, if any, unless that frame has itself
; already run out or been freed. Shared by complete / cancel.
define internal void @__staple_completion_wake(ptr %record) {
entry:
  %waiter.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 4
  %waiter = load ptr, ptr %waiter.slot
  %has = icmp ne ptr %waiter, null
  br i1 %has, label %check.live, label %done
check.live:
  %wstate = load i8, ptr %waiter
  %live = icmp ult i8 %wstate, -2
  br i1 %live, label %enqueue, label %done
enqueue:
  %sched.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 3
  %sched = load ptr, ptr %sched.slot
  store ptr null, ptr %waiter.slot
  call void @__staple_sched_enqueue(ptr %sched, ptr %waiter)
  br label %done
done:
  ret void
}

; Resolve a completion with a `%size`-byte value at `%value`. Returns 1 if the
; consumer had already abandoned the wait (the caller still owns and must drop
; the value), 0 otherwise.
define i8 @__staple_completion_complete(ptr %record, ptr %value, {{SIZE}} %size) {
entry:
  %state = load i8, ptr %record
  %pending = icmp eq i8 %state, 0
  br i1 %pending, label %claim, label %gone
gone:
  ret i8 1
claim:
  %dest = getelementptr inbounds %Completion, ptr %record, i32 1
  call void @llvm.memcpy.p0.p0.{{SIZE}}(ptr %dest, ptr %value, {{SIZE}} %size, i1 false)
  store i8 1, ptr %record
  call void @__staple_completion_disarm_cancel(ptr %record)
  call void @__staple_completion_wake(ptr %record)
  ret i8 0
}

; Cancel a completion: a registered waiter is woken with `Cancelled`.
define void @__staple_completion_cancel(ptr %record) {
entry:
  %state = load i8, ptr %record
  %pending = icmp eq i8 %state, 0
  br i1 %pending, label %do, label %done
do:
  store i8 2, ptr %record
  call void @__staple_completion_disarm_cancel(ptr %record)
  call void @__staple_completion_wake(ptr %record)
  br label %done
done:
  ret void
}

; Consumer-side abandonment: the awaiting task was cancelled while parked here.
; Runs the cancel callback, marks the completion consumer-gone, and drops the
; stale waiter registration.
define void @__staple_completion_abandon(ptr %record) {
entry:
  %state = load i8, ptr %record
  %pending = icmp eq i8 %state, 0
  br i1 %pending, label %do, label %done
do:
  call void @__staple_completion_run_cancel(ptr %record)
  store i8 3, ptr %record
  %gen.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 2
  %gen = load {{SIZE}}, ptr %gen.slot
  %gen1 = add {{SIZE}} %gen, 1
  store {{SIZE}} %gen1, ptr %gen.slot
  %waiter.slot = getelementptr inbounds %Completion, ptr %record, i32 0, i32 4
  store ptr null, ptr %waiter.slot
  br label %done
done:
  ret void
}

; Dropping an unconsumed `Wait` abandons the operation.
define void @__staple_completion_wait_drop(ptr %record) {
entry:
  call void @__staple_completion_abandon(ptr %record)
  ret void
}

; Dropping an unresolved `Resolver` cancels the operation.
define void @__staple_completion_resolver_drop(ptr %record) {
entry:
  call void @__staple_completion_cancel(ptr %record)
  ret void
}

; ---------------------------------------------------------------------------
; C-compatible completion tokens (unit-valued). These are the entry points a
; host calls, on the runtime thread, to wake a task parked on `completion_token`.

; Resolve the token's completion with `()`.
define void @__staple_completion_token_resolve(ptr %token) {
entry:
  ; A zero-byte memcpy needs a valid pointer; the record itself serves.
  %ignored = call i8 @__staple_completion_complete(ptr %token, ptr %token, {{SIZE}} 0)
  ret void
}

; Cancel the token's completion; a parked waiter is woken with `Cancelled`.
define void @__staple_completion_token_cancel(ptr %token) {
entry:
  call void @__staple_completion_cancel(ptr %token)
  ret void
}

; Release the token without resolving. An unresolved completion is cancelled;
; an already-resolved one is untouched.
define void @__staple_completion_token_release(ptr %token) {
entry:
  call void @__staple_completion_cancel(ptr %token)
  ret void
}

; ---------------------------------------------------------------------------
; `until { predicate }` — a hand-written coroutine that subscribes the predicate
; through a reaction and parks on an internal completion.

; Header (0-9) + { completion, reaction, payload, pred_code, pred_env,
; reactive_scope, runner } (10-16).
%UntilFrame = type { i8, ptr, ptr, ptr, ptr, ptr, ptr, ptr, ptr, ptr,
                     ptr, ptr, ptr, ptr, ptr, ptr, ptr }
; { pred_code, pred_env, completion, reaction }
%UntilPayload = type { ptr, ptr, ptr, ptr }

define %CoroStatus @__staple_until_resume(ptr %frame) {
entry:
  %state = load i8, ptr %frame
  %resuming = icmp eq i8 %state, 1
  br i1 %resuming, label %finish, label %subscribe

subscribe:
  ; Scheduler = the first `%TaskRecord` found walking the parent chain (the
  ; `until` frame may sit under helper coroutines before the spawned task).
  %first.parent.slot = getelementptr inbounds %CoroHeader, ptr %frame, i32 0, i32 7
  %first.parent = load ptr, ptr %first.parent.slot
  br label %walk.parent
walk.parent:
  %chain = phi ptr [ %first.parent, %subscribe ], [ %next.parent, %walk.next ]
  %chain.null = icmp eq ptr %chain, null
  br i1 %chain.null, label %no.sched, label %walk.check
walk.check:
  %chain.rec.slot = getelementptr inbounds %CoroHeader, ptr %chain, i32 0, i32 9
  %chain.rec = load ptr, ptr %chain.rec.slot
  %chain.has.rec = icmp ne ptr %chain.rec, null
  br i1 %chain.has.rec, label %rec.sched, label %walk.next
walk.next:
  %next.parent.slot = getelementptr inbounds %CoroHeader, ptr %chain, i32 0, i32 7
  %next.parent = load ptr, ptr %next.parent.slot
  br label %walk.parent
rec.sched:
  %psched.slot = getelementptr inbounds %TaskRecord, ptr %chain.rec, i32 0, i32 4
  %psched = load ptr, ptr %psched.slot
  br label %got.sched
no.sched:
  br label %got.sched
got.sched:
  %sched = phi ptr [ null, %no.sched ], [ %psched, %rec.sched ]

  ; Completion (malloc; retained for the life of the process — the reaction
  ; subscription is cleared on teardown but the struct itself is not freed).
  %comp = call ptr @malloc({{SIZE}} ptrtoint (ptr getelementptr (%Completion, ptr null, i32 1) to {{SIZE}}))
  store i8 0, ptr %comp
  %cf = getelementptr inbounds %Completion, ptr %comp, i32 0, i32 1
  store i8 0, ptr %cf
  %cg = getelementptr inbounds %Completion, ptr %comp, i32 0, i32 2
  store {{SIZE}} 0, ptr %cg
  %cs = getelementptr inbounds %Completion, ptr %comp, i32 0, i32 3
  store ptr %sched, ptr %cs
  %cw = getelementptr inbounds %Completion, ptr %comp, i32 0, i32 4
  store ptr null, ptr %cw
  %ce = getelementptr inbounds %Completion, ptr %comp, i32 0, i32 5
  store ptr null, ptr %ce
  %cfn = getelementptr inbounds %Completion, ptr %comp, i32 0, i32 6
  store ptr null, ptr %cfn
  %comp.slot = getelementptr inbounds %UntilFrame, ptr %frame, i32 0, i32 10
  store ptr %comp, ptr %comp.slot

  ; Reaction payload.
  %pl = call ptr @malloc({{SIZE}} ptrtoint (ptr getelementptr (%UntilPayload, ptr null, i32 1) to {{SIZE}}))
  %code.slot = getelementptr inbounds %UntilFrame, ptr %frame, i32 0, i32 13
  %code = load ptr, ptr %code.slot
  %env.slot = getelementptr inbounds %UntilFrame, ptr %frame, i32 0, i32 14
  %env = load ptr, ptr %env.slot
  %pl.code = getelementptr inbounds %UntilPayload, ptr %pl, i32 0, i32 0
  store ptr %code, ptr %pl.code
  %pl.env = getelementptr inbounds %UntilPayload, ptr %pl, i32 0, i32 1
  store ptr %env, ptr %pl.env
  %pl.comp = getelementptr inbounds %UntilPayload, ptr %pl, i32 0, i32 2
  store ptr %comp, ptr %pl.comp
  %pl.rxn = getelementptr inbounds %UntilPayload, ptr %pl, i32 0, i32 3
  store ptr null, ptr %pl.rxn
  %payload.slot = getelementptr inbounds %UntilFrame, ptr %frame, i32 0, i32 12
  store ptr %pl, ptr %payload.slot

  ; Create the reaction — runs the runner synchronously once (the first eval).
  %scope.slot = getelementptr inbounds %UntilFrame, ptr %frame, i32 0, i32 15
  %scope = load ptr, ptr %scope.slot
  %runner.slot = getelementptr inbounds %UntilFrame, ptr %frame, i32 0, i32 16
  %runner = load ptr, ptr %runner.slot
  %rxn = call ptr @__staple_reaction_create(ptr %scope, ptr %runner, ptr %pl,
      {{SIZE}} ptrtoint (ptr getelementptr (%UntilPayload, ptr null, i32 1) to {{SIZE}}))
  %rxn.slot = getelementptr inbounds %UntilFrame, ptr %frame, i32 0, i32 11
  store ptr %rxn, ptr %rxn.slot
  store ptr %rxn, ptr %pl.rxn

  ; Park on the completion (fast path if the synchronous first eval resolved it).
  %susp = call i8 @__staple_completion_register(ptr %comp, ptr %frame, ptr %sched)
  %do.susp = icmp ne i8 %susp, 0
  br i1 %do.susp, label %park, label %finish

park:
  store i8 1, ptr %frame
  ret %CoroStatus { i8 4, ptr null }

finish:
  %fcomp.slot = getelementptr inbounds %UntilFrame, ptr %frame, i32 0, i32 10
  %fcomp = load ptr, ptr %fcomp.slot
  %fstate = load i8, ptr %fcomp
  %completed = icmp eq i8 %fstate, 1
  store i8 -2, ptr %frame
  br i1 %completed, label %ret.done, label %ret.cancelled
ret.done:
  ret %CoroStatus { i8 0, ptr null }
ret.cancelled:
  ret %CoroStatus { i8 3, ptr null }
}

define void @__staple_until_cleanup(ptr %frame) {
entry:
  %state = load i8, ptr %frame
  %freed = icmp eq i8 %state, -1
  br i1 %freed, label %done, label %live

live:
  %rxn.slot = getelementptr inbounds %UntilFrame, ptr %frame, i32 0, i32 11
  %rxn = load ptr, ptr %rxn.slot
  %has.rxn = icmp ne ptr %rxn, null
  br i1 %has.rxn, label %clear, label %after.rxn
clear:
  ; Unsubscribe from every signal; the reaction struct itself is leaked (a
  ; proper executor-cooperative detach is a follow-on).
  call void @__staple_reaction_clear(ptr %rxn)
  br label %after.rxn
after.rxn:
  %comp.slot = getelementptr inbounds %UntilFrame, ptr %frame, i32 0, i32 10
  %comp = load ptr, ptr %comp.slot
  %has.comp = icmp ne ptr %comp, null
  br i1 %has.comp, label %abandon, label %after.comp
abandon:
  call void @__staple_completion_abandon(ptr %comp)
  br label %after.comp
after.comp:
  call void @__staple_gc_unregister_root(ptr %frame)
  store i8 -1, ptr %frame
  br label %done
done:
  ret void
}
