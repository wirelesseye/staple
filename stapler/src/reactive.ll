%ReactiveScope = type { ptr }
%Reaction = type { ptr, ptr, ptr, ptr, i1, i1 }
%Signal = type { ptr }
%Dependency = type { ptr, ptr, ptr, ptr, ptr }

@__staple_current_reaction = internal global ptr null

declare ptr @malloc({{SIZE}})
declare void @free(ptr)
declare void @__staple_gc_register_root(ptr, {{SIZE}})
declare void @__staple_gc_unregister_root(ptr)
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

define void @__staple_reaction_run(ptr %reaction) {
entry:
  %running.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 4
  %running = load i1, ptr %running.slot
  br i1 %running, label %trap, label %run
run:
  store i1 true, ptr %running.slot
  call void @__staple_reaction_clear(ptr %reaction)
  %previous = load ptr, ptr @__staple_current_reaction
  store ptr %reaction, ptr @__staple_current_reaction
  %runner.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 0
  %runner = load ptr, ptr %runner.slot
  %payload.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 1
  %payload = load ptr, ptr %payload.slot
  call void %runner(ptr %payload)
  store ptr %previous, ptr @__staple_current_reaction
  store i1 false, ptr %running.slot
  ret void
trap:
  call void @llvm.trap()
  unreachable
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
  %running.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 4
  store i1 false, ptr %running.slot
  %active.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 5
  store i1 true, ptr %active.slot
  store ptr %reaction, ptr %scope
  call void @__staple_reaction_run(ptr %reaction)
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
  %active.slot = getelementptr %Reaction, ptr %owner, i32 0, i32 5
  %active = load i1, ptr %active.slot
  br i1 %active, label %run, label %advance
run:
  call void @__staple_reaction_run(ptr %owner)
  br label %advance
advance:
  br label %loop
exit:
  ret void
}

define void @__staple_reactive_scope_dispose(ptr %scope) {
entry:
  %first = load ptr, ptr %scope
  store ptr null, ptr %scope
  br label %loop
loop:
  %reaction = phi ptr [ %first, %entry ], [ %next, %body ]
  %done = icmp eq ptr %reaction, null
  br i1 %done, label %exit, label %body
body:
  %next.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 3
  %next = load ptr, ptr %next.slot
  %active.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 5
  store i1 false, ptr %active.slot
  call void @__staple_reaction_clear(ptr %reaction)
  %payload.slot = getelementptr %Reaction, ptr %reaction, i32 0, i32 1
  %payload = load ptr, ptr %payload.slot
  call void @__staple_gc_unregister_root(ptr %payload)
  call void @free(ptr %payload)
  call void @free(ptr %reaction)
  br label %loop
exit:
  call void @free(ptr %scope)
  ret void
}
