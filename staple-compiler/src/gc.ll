; Every allocation is stored as [GcHeader | payload]. The header fields are:
; next allocation, requested payload size, mark bit, finalizer, finalized bit.
%GcHeader = type { ptr, {{SIZE}}, {{SIZE}}, ptr, {{SIZE}} }
; Explicit roots form a linked list of byte ranges: start, size, next root.
%GcRoot = type { ptr, {{SIZE}}, ptr }

; Global collector state. The hash table contains payload pointers and uses
; address 1 as a tombstone, allowing expected-O(1) pointer validation.
@__staple_gc_head = internal global ptr null
@__staple_gc_roots = internal global ptr null
@__staple_gc_bytes = internal global {{SIZE}} 0
@__staple_gc_threshold = internal global {{SIZE}} 1048576
@__staple_gc_stack_bottom = internal global ptr null
@__staple_gc_table = internal global ptr null
@__staple_gc_table_capacity = internal global {{SIZE}} 0
@__staple_gc_table_count = internal global {{SIZE}} 0
@__staple_gc_collecting = internal global i1 false
; Payload pointers sorted by address, rebuilt at the start of every mark phase,
; and the inclusive address bounds they span. Marking resolves a pointer into
; the middle of an allocation by binary search, so interior views need no
; registration.
@__staple_gc_sorted = internal global ptr null
@__staple_gc_sorted_capacity = internal global {{SIZE}} 0
@__staple_gc_sorted_count = internal global {{SIZE}} 0
@__staple_gc_heap_low = internal global {{SIZE}} 0
@__staple_gc_heap_high = internal global {{SIZE}} 0

declare ptr @malloc({{SIZE}})
declare ptr @calloc({{SIZE}}, {{SIZE}})
declare ptr @realloc(ptr, {{SIZE}})
declare void @free(ptr)
declare void @qsort(ptr, {{SIZE}}, {{SIZE}}, ptr)
declare i32 @setjmp(ptr) returns_twice
declare void @llvm.trap()

; Insert into a particular open-addressed table. The caller guarantees space.
define internal void @__staple_gc_hash_insert_into(ptr %table, {{SIZE}} %capacity, ptr %payload) {
entry:
  %integer = ptrtoint ptr %payload to {{SIZE}}
  %shifted = lshr {{SIZE}} %integer, {{PTR_SHIFT}}
  %initial = urem {{SIZE}} %shifted, %capacity
  br label %loop

loop:
  %index = phi {{SIZE}} [ %initial, %entry ], [ %next, %occupied ]
  %slot = getelementptr ptr, ptr %table, {{SIZE}} %index
  %value = load ptr, ptr %slot
  %empty = icmp eq ptr %value, null
  %tombstone = icmp eq ptr %value, inttoptr ({{SIZE}} 1 to ptr)
  %available = or i1 %empty, %tombstone
  br i1 %available, label %insert, label %occupied

occupied:
  %incremented = add {{SIZE}} %index, 1
  %next = urem {{SIZE}} %incremented, %capacity
  br label %loop

insert:
  store ptr %payload, ptr %slot
  ret void
}

; Allocate a larger table and rehash every live entry from the old table.
define internal void @__staple_gc_hash_grow() {
entry:
  %old.table = load ptr, ptr @__staple_gc_table
  %old.capacity = load {{SIZE}}, ptr @__staple_gc_table_capacity
  %uninitialized = icmp eq {{SIZE}} %old.capacity, 0
  %doubled = shl {{SIZE}} %old.capacity, 1
  %new.capacity = select i1 %uninitialized, {{SIZE}} 65536, {{SIZE}} %doubled
  %overflow = icmp eq {{SIZE}} %new.capacity, 0
  br i1 %overflow, label %trap, label %allocate

allocate:
  %new.table = call ptr @calloc({{SIZE}} %new.capacity, {{SIZE}} {{PTR_BYTES}})
  %failed = icmp eq ptr %new.table, null
  br i1 %failed, label %trap, label %install

install:
  store ptr %new.table, ptr @__staple_gc_table
  store {{SIZE}} %new.capacity, ptr @__staple_gc_table_capacity
  br i1 %uninitialized, label %return, label %rehash.loop

rehash.loop:
  %index = phi {{SIZE}} [ 0, %install ], [ %next, %rehash.next ]
  %done = icmp eq {{SIZE}} %index, %old.capacity
  br i1 %done, label %release, label %rehash.body

rehash.body:
  %old.slot = getelementptr ptr, ptr %old.table, {{SIZE}} %index
  %payload = load ptr, ptr %old.slot
  %empty = icmp eq ptr %payload, null
  %tombstone = icmp eq ptr %payload, inttoptr ({{SIZE}} 1 to ptr)
  %skip = or i1 %empty, %tombstone
  br i1 %skip, label %rehash.next, label %rehash.insert

rehash.insert:
  call void @__staple_gc_hash_insert_into(ptr %new.table, {{SIZE}} %new.capacity, ptr %payload)
  br label %rehash.next

rehash.next:
  %next = add {{SIZE}} %index, 1
  br label %rehash.loop

release:
  call void @free(ptr %old.table)
  br label %return

return:
  ret void

trap:
  call void @llvm.trap()
  unreachable
}

; Keep the table at or below a 50% load factor before inserting a payload.
define internal void @__staple_gc_hash_insert(ptr %payload) {
entry:
  %capacity = load {{SIZE}}, ptr @__staple_gc_table_capacity
  %count = load {{SIZE}}, ptr @__staple_gc_table_count
  %twice.count = shl {{SIZE}} %count, 1
  %grow = icmp uge {{SIZE}} %twice.count, %capacity
  br i1 %grow, label %resize, label %insert

resize:
  call void @__staple_gc_hash_grow()
  br label %insert

insert:
  %table = load ptr, ptr @__staple_gc_table
  %active.capacity = load {{SIZE}}, ptr @__staple_gc_table_capacity
  call void @__staple_gc_hash_insert_into(ptr %table, {{SIZE}} %active.capacity, ptr %payload)
  %old.count = load {{SIZE}}, ptr @__staple_gc_table_count
  %new.count = add {{SIZE}} %old.count, 1
  store {{SIZE}} %new.count, ptr @__staple_gc_table_count
  ret void
}

; Return the allocation whose payload begins exactly at candidate, if any.
define internal ptr @__staple_gc_hash_lookup({{SIZE}} %candidate) {
entry:
  %table = load ptr, ptr @__staple_gc_table
  %capacity = load {{SIZE}}, ptr @__staple_gc_table_capacity
  %uninitialized = icmp eq {{SIZE}} %capacity, 0
  br i1 %uninitialized, label %not.found, label %start

start:
  %shifted = lshr {{SIZE}} %candidate, {{PTR_SHIFT}}
  %initial = urem {{SIZE}} %shifted, %capacity
  br label %loop

loop:
  %index = phi {{SIZE}} [ %initial, %start ], [ %next, %continue ]
  %probes = phi {{SIZE}} [ 0, %start ], [ %next.probes, %continue ]
  %slot = getelementptr ptr, ptr %table, {{SIZE}} %index
  %payload = load ptr, ptr %slot
  %empty = icmp eq ptr %payload, null
  br i1 %empty, label %not.found, label %check

check:
  %tombstone = icmp eq ptr %payload, inttoptr ({{SIZE}} 1 to ptr)
  br i1 %tombstone, label %continue, label %compare

compare:
  %integer = ptrtoint ptr %payload to {{SIZE}}
  %matches = icmp eq {{SIZE}} %integer, %candidate
  br i1 %matches, label %found, label %continue

continue:
  %incremented = add {{SIZE}} %index, 1
  %next = urem {{SIZE}} %incremented, %capacity
  %next.probes = add {{SIZE}} %probes, 1
  %exhausted = icmp eq {{SIZE}} %next.probes, %capacity
  br i1 %exhausted, label %not.found, label %loop

found:
  ret ptr %payload

not.found:
  ret ptr null
}

; Order two payload pointers by address for qsort.
define internal i32 @__staple_gc_compare_payloads(ptr %left, ptr %right) {
entry:
  %left.value = load {{SIZE}}, ptr %left
  %right.value = load {{SIZE}}, ptr %right
  %less = icmp ult {{SIZE}} %left.value, %right.value
  %greater = icmp ugt {{SIZE}} %left.value, %right.value
  %positive = zext i1 %greater to i32
  %result = select i1 %less, i32 -1, i32 %positive
  ret i32 %result
}

; Snapshot every allocation's payload pointer in address order, and record the
; lowest payload start and the highest payload end. The array is reused across
; collections and grows to the live allocation count.
define internal void @__staple_gc_build_sorted() {
entry:
  store {{SIZE}} 0, ptr @__staple_gc_sorted_count
  %head = load ptr, ptr @__staple_gc_head
  br label %count.loop

count.loop:
  %count.header = phi ptr [ %head, %entry ], [ %count.next, %count.body ]
  %count = phi {{SIZE}} [ 0, %entry ], [ %count.incremented, %count.body ]
  %count.done = icmp eq ptr %count.header, null
  br i1 %count.done, label %ensure, label %count.body

count.body:
  %count.incremented = add {{SIZE}} %count, 1
  %count.next.slot = getelementptr %GcHeader, ptr %count.header, i32 0, i32 0
  %count.next = load ptr, ptr %count.next.slot
  br label %count.loop

ensure:
  %empty = icmp eq {{SIZE}} %count, 0
  br i1 %empty, label %return, label %capacity.check

capacity.check:
  %capacity = load {{SIZE}}, ptr @__staple_gc_sorted_capacity
  %enough = icmp uge {{SIZE}} %capacity, %count
  br i1 %enough, label %fill.start, label %grow

grow:
  ; Every counted allocation occupies more than a pointer, so the byte count
  ; cannot overflow.
  %old = load ptr, ptr @__staple_gc_sorted
  %bytes = mul {{SIZE}} %count, {{PTR_BYTES}}
  %grown = call ptr @realloc(ptr %old, {{SIZE}} %bytes)
  %failed = icmp eq ptr %grown, null
  br i1 %failed, label %trap, label %install

install:
  store ptr %grown, ptr @__staple_gc_sorted
  store {{SIZE}} %count, ptr @__staple_gc_sorted_capacity
  br label %fill.start

fill.start:
  %sorted = load ptr, ptr @__staple_gc_sorted
  br label %fill.loop

fill.loop:
  %fill.header = phi ptr [ %head, %fill.start ], [ %fill.next, %fill.body ]
  %index = phi {{SIZE}} [ 0, %fill.start ], [ %index.next, %fill.body ]
  %fill.done = icmp eq ptr %fill.header, null
  br i1 %fill.done, label %sort, label %fill.body

fill.body:
  %payload = getelementptr i8, ptr %fill.header, {{SIZE}} {{HEADER_BYTES}}
  %slot = getelementptr ptr, ptr %sorted, {{SIZE}} %index
  store ptr %payload, ptr %slot
  %index.next = add {{SIZE}} %index, 1
  %fill.next.slot = getelementptr %GcHeader, ptr %fill.header, i32 0, i32 0
  %fill.next = load ptr, ptr %fill.next.slot
  br label %fill.loop

sort:
  call void @qsort(ptr %sorted, {{SIZE}} %index, {{SIZE}} {{PTR_BYTES}}, ptr @__staple_gc_compare_payloads)
  store {{SIZE}} %index, ptr @__staple_gc_sorted_count
  %first = load {{SIZE}}, ptr %sorted
  store {{SIZE}} %first, ptr @__staple_gc_heap_low
  ; Allocations never overlap, so the last start also has the highest end.
  %last.index = sub {{SIZE}} %index, 1
  %last.slot = getelementptr ptr, ptr %sorted, {{SIZE}} %last.index
  %last = load ptr, ptr %last.slot
  %last.end = call {{SIZE}} @__staple_gc_payload_end(ptr %last)
  store {{SIZE}} %last.end, ptr @__staple_gc_heap_high
  br label %return

return:
  ret void

trap:
  call void @llvm.trap()
  unreachable
}

; The address one past a payload's requested size. A pointer equal to it still
; belongs to the allocation, so an empty view at the end stays valid.
define internal {{SIZE}} @__staple_gc_payload_end(ptr %payload) {
entry:
  %header = getelementptr i8, ptr %payload, {{SIZE}} -{{HEADER_BYTES}}
  %size.slot = getelementptr %GcHeader, ptr %header, i32 0, i32 1
  %size = load {{SIZE}}, ptr %size.slot
  %start = ptrtoint ptr %payload to {{SIZE}}
  %end = add {{SIZE}} %start, %size
  ret {{SIZE}} %end
}

; Return the allocation whose payload contains candidate, from its start through
; one past its end, if any. Only valid during a mark phase.
define internal ptr @__staple_gc_containing_payload({{SIZE}} %candidate) {
entry:
  %count = load {{SIZE}}, ptr @__staple_gc_sorted_count
  %low = load {{SIZE}}, ptr @__staple_gc_heap_low
  %high = load {{SIZE}}, ptr @__staple_gc_heap_high
  %empty = icmp eq {{SIZE}} %count, 0
  %below = icmp ult {{SIZE}} %candidate, %low
  %above = icmp ugt {{SIZE}} %candidate, %high
  %outside.bounds = or i1 %below, %above
  %outside = or i1 %empty, %outside.bounds
  br i1 %outside, label %not.found, label %search.start

search.start:
  %sorted = load ptr, ptr @__staple_gc_sorted
  br label %search.loop

; Find the first payload that starts after candidate; its predecessor is the
; only allocation that can contain candidate.
search.loop:
  %lower = phi {{SIZE}} [ 0, %search.start ], [ %lower.next, %search.body ]
  %upper = phi {{SIZE}} [ %count, %search.start ], [ %upper.next, %search.body ]
  %searching = icmp ult {{SIZE}} %lower, %upper
  br i1 %searching, label %search.body, label %search.done

search.body:
  %span = sub {{SIZE}} %upper, %lower
  %half = lshr {{SIZE}} %span, 1
  %middle = add {{SIZE}} %lower, %half
  %middle.slot = getelementptr ptr, ptr %sorted, {{SIZE}} %middle
  %middle.value = load {{SIZE}}, ptr %middle.slot
  %starts.after = icmp ugt {{SIZE}} %middle.value, %candidate
  %middle.plus = add {{SIZE}} %middle, 1
  %lower.next = select i1 %starts.after, {{SIZE}} %lower, {{SIZE}} %middle.plus
  %upper.next = select i1 %starts.after, {{SIZE}} %middle, {{SIZE}} %upper
  br label %search.loop

search.done:
  %none.before = icmp eq {{SIZE}} %lower, 0
  br i1 %none.before, label %not.found, label %check

check:
  %index = sub {{SIZE}} %lower, 1
  %slot = getelementptr ptr, ptr %sorted, {{SIZE}} %index
  %payload = load ptr, ptr %slot
  %end = call {{SIZE}} @__staple_gc_payload_end(ptr %payload)
  %inside = icmp ule {{SIZE}} %candidate, %end
  br i1 %inside, label %found, label %not.found

found:
  ret ptr %payload

not.found:
  ret ptr null
}

; Replace a removed payload with a tombstone so later probe chains stay valid.
define internal void @__staple_gc_hash_remove(ptr %payload) {
entry:
  %candidate = ptrtoint ptr %payload to {{SIZE}}
  %table = load ptr, ptr @__staple_gc_table
  %capacity = load {{SIZE}}, ptr @__staple_gc_table_capacity
  %shifted = lshr {{SIZE}} %candidate, {{PTR_SHIFT}}
  %initial = urem {{SIZE}} %shifted, %capacity
  br label %loop

loop:
  %index = phi {{SIZE}} [ %initial, %entry ], [ %next, %continue ]
  %probes = phi {{SIZE}} [ 0, %entry ], [ %next.probes, %continue ]
  %slot = getelementptr ptr, ptr %table, {{SIZE}} %index
  %value = load ptr, ptr %slot
  %integer = ptrtoint ptr %value to {{SIZE}}
  %matches = icmp eq {{SIZE}} %integer, %candidate
  br i1 %matches, label %remove, label %continue

continue:
  %incremented = add {{SIZE}} %index, 1
  %next = urem {{SIZE}} %incremented, %capacity
  %next.probes = add {{SIZE}} %probes, 1
  %exhausted = icmp eq {{SIZE}} %next.probes, %capacity
  br i1 %exhausted, label %return, label %loop

remove:
  store ptr inttoptr ({{SIZE}} 1 to ptr), ptr %slot
  %count = load {{SIZE}}, ptr @__staple_gc_table_count
  %new.count = sub {{SIZE}} %count, 1
  store {{SIZE}} %new.count, ptr @__staple_gc_table_count
  br label %return

return:
  ret void
}

; Mark the allocation an exact or interior pointer refers to, and recursively
; scan the object.
define internal void @__staple_gc_mark_candidate({{SIZE}} %candidate) {
entry:
  %exact = call ptr @__staple_gc_hash_lookup({{SIZE}} %candidate)
  %found.exact = icmp ne ptr %exact, null
  br i1 %found.exact, label %lookup.done, label %lookup.interior

lookup.interior:
  %interior = call ptr @__staple_gc_containing_payload({{SIZE}} %candidate)
  br label %lookup.done

lookup.done:
  %payload = phi ptr [ %exact, %entry ], [ %interior, %lookup.interior ]
  %found = icmp ne ptr %payload, null
  br i1 %found, label %mark.check, label %return

mark.check:
  %header = getelementptr i8, ptr %payload, {{SIZE}} -{{HEADER_BYTES}}
  %mark.slot = getelementptr %GcHeader, ptr %header, i32 0, i32 2
  %mark = load {{SIZE}}, ptr %mark.slot
  %already.marked = icmp ne {{SIZE}} %mark, 0
  br i1 %already.marked, label %return, label %mark.object

mark.object:
  store {{SIZE}} 1, ptr %mark.slot
  %size.slot = getelementptr %GcHeader, ptr %header, i32 0, i32 1
  %size = load {{SIZE}}, ptr %size.slot
  call void @__staple_gc_scan_region(ptr %payload, {{SIZE}} %size)
  br label %return

return:
  ret void
}

; Conservatively inspect every possibly unaligned pointer-sized window.
; Advancing one byte at a time catches pointers stored at arbitrary alignment.
define internal void @__staple_gc_scan_region(ptr %start, {{SIZE}} %size) {
entry:
  %large.enough = icmp uge {{SIZE}} %size, {{PTR_BYTES}}
  br i1 %large.enough, label %loop, label %return

loop:
  %offset = phi {{SIZE}} [ 0, %entry ], [ %next, %body ]
  %remaining = sub {{SIZE}} %size, %offset
  %has.word = icmp uge {{SIZE}} %remaining, {{PTR_BYTES}}
  br i1 %has.word, label %body, label %return

body:
  %slot = getelementptr i8, ptr %start, {{SIZE}} %offset
  %candidate = load {{SIZE}}, ptr %slot, align 1
  call void @__staple_gc_mark_candidate({{SIZE}} %candidate)
  %next = add {{SIZE}} %offset, 1
  br label %loop

return:
  ret void
}

; Run a conservative mark/finalize/sweep collection.
define internal void @__staple_gc_collect() {
entry:
  %stack.bottom = load ptr, ptr @__staple_gc_stack_bottom
  %initialized = icmp ne ptr %stack.bottom, null
  br i1 %initialized, label %clear.start, label %return

clear.start:
  store i1 true, ptr @__staple_gc_collecting
  %head = load ptr, ptr @__staple_gc_head
  br label %clear.loop

clear.loop:
  %clear.header = phi ptr [ %head, %clear.start ], [ %clear.next, %clear.body ]
  %clear.done = icmp eq ptr %clear.header, null
  br i1 %clear.done, label %sorted.start, label %clear.body

clear.body:
  %clear.mark = getelementptr %GcHeader, ptr %clear.header, i32 0, i32 2
  store {{SIZE}} 0, ptr %clear.mark
  %clear.next.slot = getelementptr %GcHeader, ptr %clear.header, i32 0, i32 0
  %clear.next = load ptr, ptr %clear.next.slot
  br label %clear.loop

sorted.start:
  call void @__staple_gc_build_sorted()
  br label %roots.start

roots.start:
  ; setjmp exposes saved register state in a scannable memory buffer.
  %registers = alloca [64 x {{SIZE}}], align {{PTR_BYTES}}
  %registers.pointer = getelementptr [64 x {{SIZE}}], ptr %registers, i32 0, i32 0
  %ignored = call i32 @setjmp(ptr %registers.pointer)
  call void @__staple_gc_scan_region(ptr %registers.pointer, {{SIZE}} {{REGISTER_BYTES}})
  %stack.current.integer = ptrtoint ptr %registers.pointer to {{SIZE}}
  %stack.bottom.integer = ptrtoint ptr %stack.bottom to {{SIZE}}
  ; Select the lower address so either upward- or downward-growing stacks work.
  %stack.grows.down = icmp ule {{SIZE}} %stack.current.integer, %stack.bottom.integer
  %stack.start = select i1 %stack.grows.down, ptr %registers.pointer, ptr %stack.bottom
  %stack.low = select i1 %stack.grows.down, {{SIZE}} %stack.current.integer, {{SIZE}} %stack.bottom.integer
  %stack.high = select i1 %stack.grows.down, {{SIZE}} %stack.bottom.integer, {{SIZE}} %stack.current.integer
  %stack.size = sub {{SIZE}} %stack.high, %stack.low
  call void @__staple_gc_scan_region(ptr %stack.start, {{SIZE}} %stack.size)
  %roots = load ptr, ptr @__staple_gc_roots
  br label %roots.loop

roots.loop:
  %root = phi ptr [ %roots, %roots.start ], [ %root.next, %roots.body ]
  %roots.done = icmp eq ptr %root, null
  br i1 %roots.done, label %finalize.start, label %roots.body

roots.body:
  %root.start.slot = getelementptr %GcRoot, ptr %root, i32 0, i32 0
  %root.start = load ptr, ptr %root.start.slot
  %root.size.slot = getelementptr %GcRoot, ptr %root, i32 0, i32 1
  %root.size = load {{SIZE}}, ptr %root.size.slot
  call void @__staple_gc_scan_region(ptr %root.start, {{SIZE}} %root.size)
  %root.next.slot = getelementptr %GcRoot, ptr %root, i32 0, i32 2
  %root.next = load ptr, ptr %root.next.slot
  br label %roots.loop

finalize.start:
  ; Finalizers run before sweeping, and the finalized flag prevents repeat calls.
  %finalize.head = load ptr, ptr @__staple_gc_head
  br label %finalize.loop

finalize.loop:
  %finalize.header = phi ptr [ %finalize.head, %finalize.start ], [ %finalize.next, %finalize.continue ]
  %finalize.done = icmp eq ptr %finalize.header, null
  br i1 %finalize.done, label %sweep.start, label %finalize.check

finalize.check:
  %finalize.mark.slot = getelementptr %GcHeader, ptr %finalize.header, i32 0, i32 2
  %finalize.mark = load {{SIZE}}, ptr %finalize.mark.slot
  %finalize.dead = icmp eq {{SIZE}} %finalize.mark, 0
  %finalizer.slot = getelementptr %GcHeader, ptr %finalize.header, i32 0, i32 3
  %finalizer = load ptr, ptr %finalizer.slot
  %has.finalizer = icmp ne ptr %finalizer, null
  %finalized.slot = getelementptr %GcHeader, ptr %finalize.header, i32 0, i32 4
  %finalized = load {{SIZE}}, ptr %finalized.slot
  %not.finalized = icmp eq {{SIZE}} %finalized, 0
  %needs.finalize.1 = and i1 %finalize.dead, %has.finalizer
  %needs.finalize = and i1 %needs.finalize.1, %not.finalized
  br i1 %needs.finalize, label %finalize.call, label %finalize.continue

finalize.call:
  store {{SIZE}} 1, ptr %finalized.slot
  %finalize.payload = getelementptr i8, ptr %finalize.header, {{SIZE}} {{HEADER_BYTES}}
  call void %finalizer(ptr %finalize.payload)
  br label %finalize.continue

finalize.continue:
  %finalize.next.slot = getelementptr %GcHeader, ptr %finalize.header, i32 0, i32 0
  %finalize.next = load ptr, ptr %finalize.next.slot
  br label %finalize.loop

sweep.start:
  ; %link points to the list slot that names the current object. Keeping this
  ; pointer-to-link makes removal possible without a separate previous node.
  br label %sweep.loop

sweep.loop:
  %link = phi ptr [ @__staple_gc_head, %sweep.start ], [ %next.link, %keep ], [ %link, %discard ]
  %header = load ptr, ptr %link
  %sweep.done = icmp eq ptr %header, null
  br i1 %sweep.done, label %threshold, label %sweep.check

sweep.check:
  %mark.slot = getelementptr %GcHeader, ptr %header, i32 0, i32 2
  %mark = load {{SIZE}}, ptr %mark.slot
  %live = icmp ne {{SIZE}} %mark, 0
  br i1 %live, label %keep, label %discard

keep:
  %next.link = getelementptr %GcHeader, ptr %header, i32 0, i32 0
  br label %sweep.loop

discard:
  %dead.next.slot = getelementptr %GcHeader, ptr %header, i32 0, i32 0
  %dead.next = load ptr, ptr %dead.next.slot
  store ptr %dead.next, ptr %link
  %dead.size.slot = getelementptr %GcHeader, ptr %header, i32 0, i32 1
  %dead.size = load {{SIZE}}, ptr %dead.size.slot
  %dead.payload = getelementptr i8, ptr %header, {{SIZE}} {{HEADER_BYTES}}
  call void @__staple_gc_hash_remove(ptr %dead.payload)
  %dead.has.payload = icmp ne {{SIZE}} %dead.size, 0
  %dead.payload.size = select i1 %dead.has.payload, {{SIZE}} %dead.size, {{SIZE}} 1
  %dead.charge = add {{SIZE}} %dead.payload.size, {{HEADER_BYTES}}
  %bytes = load {{SIZE}}, ptr @__staple_gc_bytes
  %remaining.bytes = sub {{SIZE}} %bytes, %dead.charge
  store {{SIZE}} %remaining.bytes, ptr @__staple_gc_bytes
  call void @free(ptr %header)
  br label %sweep.loop

threshold:
  ; Aim for at least 100% headroom after collection, with a 1 MiB floor.
  %live.bytes = load {{SIZE}}, ptr @__staple_gc_bytes
  %can.double = icmp ule {{SIZE}} %live.bytes, {{MAX_HALF}}
  %doubled = shl {{SIZE}} %live.bytes, 1
  %grown = select i1 %can.double, {{SIZE}} %doubled, {{SIZE}} {{MAX}}
  %small = icmp ult {{SIZE}} %grown, 1048576
  %next.threshold = select i1 %small, {{SIZE}} 1048576, {{SIZE}} %grown
  store {{SIZE}} %next.threshold, ptr @__staple_gc_threshold
  store i1 false, ptr @__staple_gc_collecting
  br label %return

return:
  ret void
}

; Allocate a header and payload as one block, guarding the size addition.
define internal ptr @__staple_gc_try_allocate({{SIZE}} %size) {
entry:
  %has.payload = icmp ne {{SIZE}} %size, 0
  %payload.size = select i1 %has.payload, {{SIZE}} %size, {{SIZE}} 1
  %fits = icmp ule {{SIZE}} %payload.size, {{MAX_ALLOC}}
  br i1 %fits, label %allocate, label %trap

allocate:
  %total = add {{SIZE}} %payload.size, {{HEADER_BYTES}}
  %header = call ptr @malloc({{SIZE}} %total)
  ret ptr %header

trap:
  call void @llvm.trap()
  unreachable
}

; Allocate a managed object, collecting at the threshold and retrying once on
; malloc failure. Zero-byte requests receive one physical payload byte.
define ptr @__staple_gc_alloc({{SIZE}} %size) {
entry:
  %has.payload = icmp ne {{SIZE}} %size, 0
  %payload.size = select i1 %has.payload, {{SIZE}} %size, {{SIZE}} 1
  %fits = icmp ule {{SIZE}} %payload.size, {{MAX_ALLOC}}
  br i1 %fits, label %threshold.check, label %trap

threshold.check:
  %charge = add {{SIZE}} %payload.size, {{HEADER_BYTES}}
  %bytes = load {{SIZE}}, ptr @__staple_gc_bytes
  %threshold = load {{SIZE}}, ptr @__staple_gc_threshold
  %at.limit = icmp uge {{SIZE}} %bytes, %threshold
  %remaining = sub {{SIZE}} %threshold, %bytes
  %exceeds.remaining = icmp ugt {{SIZE}} %charge, %remaining
  %would.cross = or i1 %at.limit, %exceeds.remaining
  %collecting = load i1, ptr @__staple_gc_collecting
  %can.collect = xor i1 %collecting, true
  %crosses = and i1 %would.cross, %can.collect
  br i1 %crosses, label %collect.first, label %allocate.first

collect.first:
  call void @__staple_gc_collect()
  br label %allocate.first

allocate.first:
  %first = call ptr @__staple_gc_try_allocate({{SIZE}} %size)
  %first.failed = icmp eq ptr %first, null
  br i1 %first.failed, label %retry, label %initialize

retry:
  %retry.collecting = load i1, ptr @__staple_gc_collecting
  br i1 %retry.collecting, label %trap, label %retry.collect

retry.collect:
  call void @__staple_gc_collect()
  %second = call ptr @__staple_gc_try_allocate({{SIZE}} %size)
  %second.failed = icmp eq ptr %second, null
  br i1 %second.failed, label %trap, label %initialize

initialize:
  %header = phi ptr [ %first, %allocate.first ], [ %second, %retry.collect ]
  %head = load ptr, ptr @__staple_gc_head
  %next.slot = getelementptr %GcHeader, ptr %header, i32 0, i32 0
  store ptr %head, ptr %next.slot
  %size.slot = getelementptr %GcHeader, ptr %header, i32 0, i32 1
  store {{SIZE}} %size, ptr %size.slot
  %mark.slot = getelementptr %GcHeader, ptr %header, i32 0, i32 2
  ; Objects allocated by a finalizer are live for the collection in progress.
  %initial.mark.bit = load i1, ptr @__staple_gc_collecting
  %initial.mark = zext i1 %initial.mark.bit to {{SIZE}}
  store {{SIZE}} %initial.mark, ptr %mark.slot
  %finalizer.slot = getelementptr %GcHeader, ptr %header, i32 0, i32 3
  store ptr null, ptr %finalizer.slot
  %finalized.slot = getelementptr %GcHeader, ptr %header, i32 0, i32 4
  store {{SIZE}} 0, ptr %finalized.slot
  store ptr %header, ptr @__staple_gc_head
  %old.bytes = load {{SIZE}}, ptr @__staple_gc_bytes
  %new.bytes = add {{SIZE}} %old.bytes, %charge
  store {{SIZE}} %new.bytes, ptr @__staple_gc_bytes
  %payload = getelementptr i8, ptr %header, {{SIZE}} {{HEADER_BYTES}}
  call void @__staple_gc_hash_insert(ptr %payload)
  ret ptr %payload

trap:
  call void @llvm.trap()
  unreachable
}

; Attach or replace the callback invoked when payload first becomes unreachable.
define void @__staple_gc_set_finalizer(ptr %payload, ptr %finalizer) {
entry:
  %header = getelementptr i8, ptr %payload, {{SIZE}} -{{HEADER_BYTES}}
  %slot = getelementptr %GcHeader, ptr %header, i32 0, i32 3
  store ptr %finalizer, ptr %slot
  ret void
}

; Record the stack boundary supplied by the program's startup code.
define void @__staple_gc_set_stack_bottom(ptr %bottom) {
entry:
  store ptr %bottom, ptr @__staple_gc_stack_bottom
  ret void
}

; Permanently register an additional byte range for conservative root scanning.
define void @__staple_gc_register_root(ptr %start, {{SIZE}} %size) {
entry:
  %node = call ptr @malloc({{SIZE}} {{ROOT_BYTES}})
  %failed = icmp eq ptr %node, null
  br i1 %failed, label %trap, label %initialize

initialize:
  %start.slot = getelementptr %GcRoot, ptr %node, i32 0, i32 0
  store ptr %start, ptr %start.slot
  %size.slot = getelementptr %GcRoot, ptr %node, i32 0, i32 1
  store {{SIZE}} %size, ptr %size.slot
  %roots = load ptr, ptr @__staple_gc_roots
  %next.slot = getelementptr %GcRoot, ptr %node, i32 0, i32 2
  store ptr %roots, ptr %next.slot
  store ptr %node, ptr @__staple_gc_roots
  ret void

trap:
  call void @llvm.trap()
  unreachable
}

; Removes a previously registered root range.
define void @__staple_gc_unregister_root(ptr %start) {
entry:
  %first = load ptr, ptr @__staple_gc_roots
  br label %loop

loop:
  %previous = phi ptr [ null, %entry ], [ %node, %advance ]
  %node = phi ptr [ %first, %entry ], [ %next, %advance ]
  %done = icmp eq ptr %node, null
  br i1 %done, label %exit, label %check

check:
  %start.slot = getelementptr %GcRoot, ptr %node, i32 0, i32 0
  %candidate = load ptr, ptr %start.slot
  %matches = icmp eq ptr %candidate, %start
  %next.slot = getelementptr %GcRoot, ptr %node, i32 0, i32 2
  %next = load ptr, ptr %next.slot
  br i1 %matches, label %remove, label %advance

remove:
  %at.head = icmp eq ptr %previous, null
  br i1 %at.head, label %remove.head, label %remove.after

remove.head:
  store ptr %next, ptr @__staple_gc_roots
  br label %release

remove.after:
  %previous.next = getelementptr %GcRoot, ptr %previous, i32 0, i32 2
  store ptr %next, ptr %previous.next
  br label %release

release:
  call void @free(ptr %node)
  ret void

advance:
  br label %loop

exit:
  ret void
}
