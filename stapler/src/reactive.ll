%ReactiveScope = type { ptr }
%Reaction = type { ptr, ptr, ptr, ptr, i8, i1, ptr, ptr, {{SIZE}} }
%Signal = type { ptr }
%Dependency = type { ptr, ptr, ptr, ptr, ptr }
%Executor = type { ptr, ptr, i1 }
%Work = type { i8, ptr, ptr }

; Reaction states. Derived computations use Idle and Running only.
; 0 = Idle, 1 = Staged, 2 = Queued, 3 = Running,
; 4 = RunningQueued, 5 = Disposed.
@__staple_current_reaction = internal global ptr null
@__staple_notify_depth = internal global i32 0
@__staple_batch_depth = internal global i32 0
@__staple_pending_reactions = internal global ptr null
@__staple_next_reaction_ordinal = internal global {{SIZE}} 0
@__staple_executor = internal global %Executor zeroinitializer
@__staple_reactive_nonconvergence = private unnamed_addr constant [58 x i8] c"reactive update did not stabilize after 100000 executions\0A"

declare ptr @malloc({{SIZE}})
declare void @free(ptr)
declare void @__staple_gc_register_root(ptr, {{SIZE}})
declare void @__staple_gc_unregister_root(ptr)
declare {{SIZE}} @write(i32, ptr, {{SIZE}})
declare void @llvm.trap()

define ptr @__staple_reactive_scope_create() {
entry:
  %scope = call ptr @malloc({{SIZE}} {{SCOPE_BYTES}})
  %failed = icmp eq ptr %scope, null
  br i1 %failed, label %trap, label %ok
ok:
  store ptr null, ptr %scope
  ret ptr %scope
trap:
  call void @llvm.trap()
  unreachable
}

define ptr @__staple_signal_create() {
entry:
  %signal = call ptr @malloc({{SIZE}} {{SIGNAL_BYTES}})
  %failed = icmp eq ptr %signal, null
  br i1 %failed, label %trap, label %ok
ok:
  store ptr null, ptr %signal
  ret ptr %signal
trap:
  call void @llvm.trap()
  unreachable
}

define void @__staple_reaction_clear(ptr %reaction) {
entry:
  %deps.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 2
  %first = load ptr, ptr %deps.slot
  store ptr null, ptr %deps.slot
  br label %loop
loop:
  %dep = phi ptr [ %first, %entry ], [ %next.reaction, %advance ]
  %done = icmp eq ptr %dep, null
  br i1 %done, label %exit, label %body
body:
  %next.reaction.slot = getelementptr %Dependency, ptr %dep, i32 0, i32 1
  %next.reaction = load ptr, ptr %next.reaction.slot
  %prev.slot = getelementptr %Dependency, ptr %dep, i32 0, i32 2
  %prev = load ptr, ptr %prev.slot
  %next.signal.slot = getelementptr %Dependency, ptr %dep, i32 0, i32 3
  %next.signal = load ptr, ptr %next.signal.slot
  %has.prev = icmp ne ptr %prev, null
  br i1 %has.prev, label %unlink.prev, label %unlink.head
unlink.prev:
  %prev.next.slot = getelementptr %Dependency, ptr %prev, i32 0, i32 3
  store ptr %next.signal, ptr %prev.next.slot
  br label %unlink.next
unlink.head:
  %signal.slot = getelementptr %Dependency, ptr %dep, i32 0, i32 0
  %signal = load ptr, ptr %signal.slot
  store ptr %next.signal, ptr %signal
  br label %unlink.next
unlink.next:
  %has.next = icmp ne ptr %next.signal, null
  br i1 %has.next, label %set.prev, label %advance
set.prev:
  %next.prev.slot = getelementptr %Dependency, ptr %next.signal, i32 0, i32 2
  store ptr %prev, ptr %next.prev.slot
  br label %advance
advance:
  call void @free(ptr %dep)
  br label %loop
exit:
  ret void
}

define void @__staple_computation_invoke(ptr %reaction) {
entry:
  call void @__staple_reaction_clear(ptr %reaction)
  %previous = load ptr, ptr @__staple_current_reaction
  store ptr %reaction, ptr @__staple_current_reaction
  %runner.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 0
  %runner = load ptr, ptr %runner.slot
  %payload.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 1
  %payload = load ptr, ptr %payload.slot
  call void %runner(ptr %payload)
  store ptr %previous, ptr @__staple_current_reaction
  ret void
}

define void @__staple_derived_run(ptr %derived) {
entry:
  %state.slot = getelementptr %Reaction, ptr %derived, i32 0, i32 4
  %state = load i8, ptr %state.slot
  %running = icmp eq i8 %state, 3
  br i1 %running, label %trap, label %run
run:
  store i8 3, ptr %state.slot
  call void @__staple_computation_invoke(ptr %derived)
  store i8 0, ptr %state.slot
  ret void
trap:
  call void @llvm.trap()
  unreachable
}

define void @__staple_reaction_destroy(ptr %reaction) {
entry:
  %payload.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 1
  %payload = load ptr, ptr %payload.slot
  call void @__staple_gc_unregister_root(ptr %payload)
  call void @free(ptr %payload)
  call void @free(ptr %reaction)
  ret void
}

define void @__staple_executor_enqueue_reaction(ptr %reaction) {
entry:
  %work = call ptr @malloc({{SIZE}} {{WORK_BYTES}})
  %failed = icmp eq ptr %work, null
  br i1 %failed, label %trap, label %initialize
initialize:
  store i8 0, ptr %work
  %target.slot = getelementptr %Work, ptr %work, i32 0, i32 1
  store ptr %reaction, ptr %target.slot
  %next.slot = getelementptr %Work, ptr %work, i32 0, i32 2
  store ptr null, ptr %next.slot
  %head.slot = getelementptr %Executor, ptr @__staple_executor, i32 0, i32 0
  %tail.slot = getelementptr %Executor, ptr @__staple_executor, i32 0, i32 1
  %tail = load ptr, ptr %tail.slot
  %empty = icmp eq ptr %tail, null
  br i1 %empty, label %install.head, label %append
install.head:
  store ptr %work, ptr %head.slot
  br label %install.tail
append:
  %tail.next.slot = getelementptr %Work, ptr %tail, i32 0, i32 2
  store ptr %work, ptr %tail.next.slot
  br label %install.tail
install.tail:
  store ptr %work, ptr %tail.slot
  ret void
trap:
  call void @llvm.trap()
  unreachable
}

; Sort staging by creation ordinal before appending it to the FIFO executor.
define void @__staple_reaction_stage(ptr %reaction) {
entry:
  %ordinal.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 8
  %ordinal = load {{SIZE}}, ptr %ordinal.slot
  %first = load ptr, ptr @__staple_pending_reactions
  br label %scan
scan:
  %previous = phi ptr [ null, %entry ], [ %current, %advance ]
  %current = phi ptr [ %first, %entry ], [ %next, %advance ]
  %done = icmp eq ptr %current, null
  br i1 %done, label %insert, label %compare
compare:
  %current.ordinal.slot = getelementptr %Reaction, ptr %current, i32 0, i32 8
  %current.ordinal = load {{SIZE}}, ptr %current.ordinal.slot
  %before = icmp ult {{SIZE}} %ordinal, %current.ordinal
  br i1 %before, label %insert, label %advance
advance:
  %current.next.slot = getelementptr %Reaction, ptr %current, i32 0, i32 7
  %next = load ptr, ptr %current.next.slot
  br label %scan
insert:
  %next.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 7
  store ptr %current, ptr %next.slot
  %at.head = icmp eq ptr %previous, null
  br i1 %at.head, label %install.head, label %install.after
install.head:
  store ptr %reaction, ptr @__staple_pending_reactions
  ret void
install.after:
  %previous.next.slot = getelementptr %Reaction, ptr %previous, i32 0, i32 7
  store ptr %reaction, ptr %previous.next.slot
  ret void
}

define void @__staple_reaction_schedule(ptr %reaction) {
entry:
  %state.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 4
  %state = load i8, ptr %state.slot
  %idle = icmp eq i8 %state, 0
  br i1 %idle, label %stage, label %check.running
stage:
  store i8 1, ptr %state.slot
  call void @__staple_reaction_stage(ptr %reaction)
  ret void
check.running:
  %running = icmp eq i8 %state, 3
  br i1 %running, label %rerun, label %return
rerun:
  store i8 4, ptr %state.slot
  ret void
return:
  ret void
}

define void @__staple_reaction_commit_pending() {
entry:
  br label %loop
loop:
  %reaction = load ptr, ptr @__staple_pending_reactions
  %done = icmp eq ptr %reaction, null
  br i1 %done, label %exit, label %pop
pop:
  %next.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 7
  %next = load ptr, ptr %next.slot
  store ptr %next, ptr @__staple_pending_reactions
  store ptr null, ptr %next.slot
  %state.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 4
  %state = load i8, ptr %state.slot
  %staged = icmp eq i8 %state, 1
  br i1 %staged, label %enqueue, label %check.disposed
enqueue:
  store i8 2, ptr %state.slot
  call void @__staple_executor_enqueue_reaction(ptr %reaction)
  br label %loop
check.disposed:
  %disposed = icmp eq i8 %state, 5
  br i1 %disposed, label %destroy, label %loop
destroy:
  call void @__staple_reaction_destroy(ptr %reaction)
  br label %loop
exit:
  ret void
}

define void @__staple_reaction_execute(ptr %reaction) {
entry:
  %state.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 4
  store i8 3, ptr %state.slot
  call void @__staple_computation_invoke(ptr %reaction)
  %state = load i8, ptr %state.slot
  %rerun = icmp eq i8 %state, 4
  br i1 %rerun, label %enqueue, label %check.disposed
enqueue:
  store i8 2, ptr %state.slot
  call void @__staple_executor_enqueue_reaction(ptr %reaction)
  ret void
check.disposed:
  %disposed = icmp eq i8 %state, 5
  br i1 %disposed, label %destroy, label %idle
idle:
  store i8 0, ptr %state.slot
  ret void
destroy:
  call void @__staple_reaction_destroy(ptr %reaction)
  ret void
}

define void @__staple_reactive_nonconvergence_trap() {
entry:
  %message = getelementptr [58 x i8], ptr @__staple_reactive_nonconvergence, i32 0, i32 0
  %written = call {{SIZE}} @write(i32 2, ptr %message, {{SIZE}} 58)
  call void @llvm.trap()
  unreachable
}

define void @__staple_executor_checkpoint() {
entry:
  %draining.slot = getelementptr %Executor, ptr @__staple_executor, i32 0, i32 2
  %draining = load i1, ptr %draining.slot
  br i1 %draining, label %return, label %start
start:
  store i1 true, ptr %draining.slot
  br label %loop
loop:
  %steps = phi {{SIZE}} [ 0, %start ], [ %next.steps, %continue ]
  %head.slot = getelementptr %Executor, ptr @__staple_executor, i32 0, i32 0
  %work = load ptr, ptr %head.slot
  %done = icmp eq ptr %work, null
  br i1 %done, label %exit, label %pop
pop:
  %next.slot = getelementptr %Work, ptr %work, i32 0, i32 2
  %next = load ptr, ptr %next.slot
  store ptr %next, ptr %head.slot
  %now.empty = icmp eq ptr %next, null
  br i1 %now.empty, label %clear.tail, label %dispatch
clear.tail:
  %tail.slot = getelementptr %Executor, ptr @__staple_executor, i32 0, i32 1
  store ptr null, ptr %tail.slot
  br label %dispatch
dispatch:
  %kind = load i8, ptr %work
  %reaction.kind = icmp eq i8 %kind, 0
  br i1 %reaction.kind, label %reaction, label %invalid
reaction:
  %target.slot = getelementptr %Work, ptr %work, i32 0, i32 1
  %target = load ptr, ptr %target.slot
  call void @free(ptr %work)
  %state.slot = getelementptr %Reaction, ptr %target, i32 0, i32 4
  %state = load i8, ptr %state.slot
  %disposed = icmp eq i8 %state, 5
  br i1 %disposed, label %destroy, label %check.ready
destroy:
  call void @__staple_reaction_destroy(ptr %target)
  br label %continue
check.ready:
  %ready = icmp eq i8 %state, 2
  br i1 %ready, label %check.budget, label %continue
check.budget:
  %exhausted = icmp uge {{SIZE}} %steps, 100000
  br i1 %exhausted, label %nonconvergent, label %run
run:
  call void @__staple_reaction_execute(ptr %target)
  %incremented = add {{SIZE}} %steps, 1
  br label %continue
continue:
  %next.steps = phi {{SIZE}} [ %steps, %destroy ], [ %steps, %check.ready ], [ %incremented, %run ]
  br label %loop
nonconvergent:
  call void @__staple_reactive_nonconvergence_trap()
  unreachable
invalid:
  call void @free(ptr %work)
  call void @llvm.trap()
  unreachable
exit:
  store i1 false, ptr %draining.slot
  br label %return
return:
  ret void
}

define void @__staple_reaction_create(ptr %scope, ptr %runner, ptr %payload, {{SIZE}} %payload.size) {
entry:
  call void @__staple_gc_register_root(ptr %payload, {{SIZE}} %payload.size)
  %reaction = call ptr @malloc({{SIZE}} {{REACTION_BYTES}})
  %failed = icmp eq ptr %reaction, null
  br i1 %failed, label %trap, label %ok
ok:
  store ptr %runner, ptr %reaction
  %payload.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 1
  store ptr %payload, ptr %payload.slot
  %deps.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 2
  store ptr null, ptr %deps.slot
  %scope.first = load ptr, ptr %scope
  %next.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 3
  store ptr %scope.first, ptr %next.slot
  %state.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 4
  store i8 0, ptr %state.slot
  %dirty.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 5
  store i1 false, ptr %dirty.slot
  %signal.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 6
  store ptr null, ptr %signal.slot
  %pending.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 7
  store ptr null, ptr %pending.slot
  %ordinal = load {{SIZE}}, ptr @__staple_next_reaction_ordinal
  %next.ordinal = add {{SIZE}} %ordinal, 1
  store {{SIZE}} %next.ordinal, ptr @__staple_next_reaction_ordinal
  %ordinal.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 8
  store {{SIZE}} %ordinal, ptr %ordinal.slot
  store ptr %reaction, ptr %scope
  call void @__staple_reaction_schedule(ptr %reaction)
  call void @__staple_reaction_commit_pending()
  %batch.depth = load i32, ptr @__staple_batch_depth
  %batched = icmp ne i32 %batch.depth, 0
  br i1 %batched, label %return, label %checkpoint
checkpoint:
  call void @__staple_executor_checkpoint()
  br label %return
return:
  ret void
trap:
  call void @llvm.trap()
  unreachable
}

define void @__staple_signal_track(ptr %signal) {
entry:
  %reaction = load ptr, ptr @__staple_current_reaction
  %inactive = icmp eq ptr %reaction, null
  br i1 %inactive, label %exit, label %scan.start
scan.start:
  %deps.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 2
  %first = load ptr, ptr %deps.slot
  br label %scan
scan:
  %candidate = phi ptr [ %first, %scan.start ], [ %next, %scan.advance ]
  %done = icmp eq ptr %candidate, null
  br i1 %done, label %add, label %check
check:
  %candidate.signal.slot = getelementptr %Dependency, ptr %candidate, i32 0, i32 0
  %candidate.signal = load ptr, ptr %candidate.signal.slot
  %same = icmp eq ptr %candidate.signal, %signal
  br i1 %same, label %exit, label %scan.advance
scan.advance:
  %next.slot = getelementptr %Dependency, ptr %candidate, i32 0, i32 1
  %next = load ptr, ptr %next.slot
  br label %scan
add:
  %dep = call ptr @malloc({{SIZE}} {{DEP_BYTES}})
  %failed = icmp eq ptr %dep, null
  br i1 %failed, label %trap, label %initialize
initialize:
  store ptr %signal, ptr %dep
  %next.reaction.slot = getelementptr %Dependency, ptr %dep, i32 0, i32 1
  store ptr %first, ptr %next.reaction.slot
  %prev.slot = getelementptr %Dependency, ptr %dep, i32 0, i32 2
  store ptr null, ptr %prev.slot
  %signal.first = load ptr, ptr %signal
  %next.signal.slot = getelementptr %Dependency, ptr %dep, i32 0, i32 3
  store ptr %signal.first, ptr %next.signal.slot
  %owner.slot = getelementptr %Dependency, ptr %dep, i32 0, i32 4
  store ptr %reaction, ptr %owner.slot
  %has.first = icmp ne ptr %signal.first, null
  br i1 %has.first, label %set.prev, label %link
set.prev:
  %first.prev.slot = getelementptr %Dependency, ptr %signal.first, i32 0, i32 2
  store ptr %dep, ptr %first.prev.slot
  br label %link
link:
  store ptr %dep, ptr %signal
  store ptr %dep, ptr %deps.slot
  br label %exit
trap:
  call void @llvm.trap()
  unreachable
exit:
  ret void
}

define void @__staple_signal_notify(ptr %signal) {
entry:
  %depth = load i32, ptr @__staple_notify_depth
  %next.depth = add i32 %depth, 1
  store i32 %next.depth, ptr @__staple_notify_depth
  %first = load ptr, ptr %signal
  br label %loop
loop:
  %dep = phi ptr [ %first, %entry ], [ %next, %advance ]
  %done = icmp eq ptr %dep, null
  br i1 %done, label %exit, label %body
body:
  %next.slot = getelementptr %Dependency, ptr %dep, i32 0, i32 3
  %next = load ptr, ptr %next.slot
  %owner.slot = getelementptr %Dependency, ptr %dep, i32 0, i32 4
  %owner = load ptr, ptr %owner.slot
  %derived.signal.slot = getelementptr %Reaction, ptr %owner, i32 0, i32 6
  %derived.signal = load ptr, ptr %derived.signal.slot
  %is.derived = icmp ne ptr %derived.signal, null
  br i1 %is.derived, label %invalidate.derived, label %schedule
invalidate.derived:
  %dirty.slot = getelementptr %Reaction, ptr %owner, i32 0, i32 5
  %dirty = load i1, ptr %dirty.slot
  br i1 %dirty, label %advance, label %mark.dirty
mark.dirty:
  store i1 true, ptr %dirty.slot
  call void @__staple_signal_notify(ptr %derived.signal)
  br label %advance
schedule:
  call void @__staple_reaction_schedule(ptr %owner)
  br label %advance
advance:
  br label %loop
exit:
  %exit.depth = load i32, ptr @__staple_notify_depth
  %remaining = sub i32 %exit.depth, 1
  store i32 %remaining, ptr @__staple_notify_depth
  %outermost = icmp eq i32 %remaining, 0
  br i1 %outermost, label %commit, label %return
commit:
  call void @__staple_reaction_commit_pending()
  %batch.depth = load i32, ptr @__staple_batch_depth
  %batched = icmp ne i32 %batch.depth, 0
  br i1 %batched, label %return, label %checkpoint
checkpoint:
  call void @__staple_executor_checkpoint()
  br label %return
return:
  ret void
}

define void @__staple_batch_begin() {
entry:
  %depth = load i32, ptr @__staple_batch_depth
  %next = add i32 %depth, 1
  store i32 %next, ptr @__staple_batch_depth
  ret void
}

define void @__staple_batch_end() {
entry:
  %depth = load i32, ptr @__staple_batch_depth
  %valid = icmp sgt i32 %depth, 0
  br i1 %valid, label %decrement, label %trap
decrement:
  %remaining = sub i32 %depth, 1
  store i32 %remaining, ptr @__staple_batch_depth
  %outermost = icmp eq i32 %remaining, 0
  br i1 %outermost, label %checkpoint, label %return
checkpoint:
  call void @__staple_reaction_commit_pending()
  call void @__staple_executor_checkpoint()
  br label %return
return:
  ret void
trap:
  call void @llvm.trap()
  unreachable
}

define ptr @__staple_derived_create(ptr %runner, ptr %payload, {{SIZE}} %payload.size) {
entry:
  call void @__staple_gc_register_root(ptr %payload, {{SIZE}} %payload.size)
  %derived = call ptr @malloc({{SIZE}} {{REACTION_BYTES}})
  %failed = icmp eq ptr %derived, null
  br i1 %failed, label %trap, label %ok
ok:
  store ptr %runner, ptr %derived
  %payload.slot = getelementptr %Reaction, ptr %derived, i32 0, i32 1
  store ptr %payload, ptr %payload.slot
  %deps.slot = getelementptr %Reaction, ptr %derived, i32 0, i32 2
  store ptr null, ptr %deps.slot
  %next.slot = getelementptr %Reaction, ptr %derived, i32 0, i32 3
  store ptr null, ptr %next.slot
  %state.slot = getelementptr %Reaction, ptr %derived, i32 0, i32 4
  store i8 0, ptr %state.slot
  %dirty.slot = getelementptr %Reaction, ptr %derived, i32 0, i32 5
  store i1 true, ptr %dirty.slot
  %signal = call ptr @__staple_signal_create()
  %signal.slot = getelementptr %Reaction, ptr %derived, i32 0, i32 6
  store ptr %signal, ptr %signal.slot
  %pending.slot = getelementptr %Reaction, ptr %derived, i32 0, i32 7
  store ptr null, ptr %pending.slot
  %ordinal.slot = getelementptr %Reaction, ptr %derived, i32 0, i32 8
  store {{SIZE}} 0, ptr %ordinal.slot
  ret ptr %derived
trap:
  call void @llvm.trap()
  unreachable
}

define void @__staple_derived_read(ptr %derived) {
entry:
  %signal.slot = getelementptr %Reaction, ptr %derived, i32 0, i32 6
  %signal = load ptr, ptr %signal.slot
  call void @__staple_signal_track(ptr %signal)
  %dirty.slot = getelementptr %Reaction, ptr %derived, i32 0, i32 5
  %dirty = load i1, ptr %dirty.slot
  br i1 %dirty, label %evaluate, label %exit
evaluate:
  call void @__staple_derived_run(ptr %derived)
  store i1 false, ptr %dirty.slot
  br label %exit
exit:
  ret void
}

define ptr @__staple_tracking_suspend() {
entry:
  %previous = load ptr, ptr @__staple_current_reaction
  store ptr null, ptr @__staple_current_reaction
  ret ptr %previous
}

define void @__staple_tracking_restore(ptr %previous) {
entry:
  store ptr %previous, ptr @__staple_current_reaction
  ret void
}

define void @__staple_reactive_scope_dispose(ptr %scope) {
entry:
  %first = load ptr, ptr %scope
  store ptr null, ptr %scope
  br label %loop
loop:
  %reaction = phi ptr [ %first, %entry ], [ %next, %continue ]
  %done = icmp eq ptr %reaction, null
  br i1 %done, label %exit, label %body
body:
  %next.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 3
  %next = load ptr, ptr %next.slot
  %state.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 4
  %state = load i8, ptr %state.slot
  store i8 5, ptr %state.slot
  call void @__staple_reaction_clear(ptr %reaction)
  %idle = icmp eq i8 %state, 0
  br i1 %idle, label %destroy, label %continue
destroy:
  call void @__staple_reaction_destroy(ptr %reaction)
  br label %continue
continue:
  br label %loop
exit:
  call void @free(ptr %scope)
  ret void
}
