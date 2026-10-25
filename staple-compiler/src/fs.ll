; POSIX filesystem bridge. Layout substitutions are restricted to supported 64-bit targets.
declare ptr @{{ERRNO}}()
declare i32 @open(ptr, i32, ...)
declare ptr @fdopen(i32, ptr)
declare i32 @close(i32)
declare i32 @fileno(ptr)
declare i32 @rename(ptr, ptr)
declare ptr @opendir(ptr)
declare ptr @{{READDIR}}(ptr)
declare i32 @closedir(ptr)
declare {{SIZE}} @strlen(ptr)
{{STAT_DECLS}}
@__staple_fs_empty = private constant [1 x i8] zeroinitializer
@__staple_fs_mode_r = private constant [2 x i8] c"r\00"
@__staple_fs_mode_w = private constant [2 x i8] c"w\00"
@__staple_fs_mode_rw = private constant [3 x i8] c"r+\00"
define i32 @__staple_fs_errno() {
  %p = call ptr @{{ERRNO}}()
  %n = load i32, ptr %p
  ret i32 %n
}
define i32 @__staple_fs_errno_value(i32 %which) {
  switch i32 %which, label %other [i32 0, label %e0 i32 1, label %e1 i32 2, label %e2 i32 3, label %e3 i32 4, label %e4 i32 5, label %e5 i32 6, label %e6 i32 7, label %e7]
e0: ret i32 2
e1: ret i32 13
e2: ret i32 17
e3: ret i32 20
e4: ret i32 21
e5: ret i32 {{ENOTEMPTY}}
e6: ret i32 {{EILSEQ}}
e7: ret i32 1
other: ret i32 22
}
define i32 @__staple_fs_rename(ptr %a, ptr %b) {
  %result = call i32 @rename(ptr %a, ptr %b)
  ret i32 %result
}
define ptr @__staple_fs_open(ptr %path, i32 %bits) {
  %access = and i32 %bits, 3
  %rdwr = icmp eq i32 %access, 3
  %write = icmp eq i32 %access, 2
  %wr = select i1 %write, i32 1, i32 0
  %base = select i1 %rdwr, i32 2, i32 %wr
  %a = and i32 %bits, 4
  %ac = icmp ne i32 %a, 0
  %av = select i1 %ac, i32 {{O_APPEND}}, i32 0
  %c = and i32 %bits, 8
  %cc = icmp ne i32 %c, 0
  %cv = select i1 %cc, i32 {{O_CREAT}}, i32 0
  %t = and i32 %bits, 16
  %tc = icmp ne i32 %t, 0
  %tv = select i1 %tc, i32 {{O_TRUNC}}, i32 0
  %x = and i32 %bits, 32
  %xc = icmp ne i32 %x, 0
  %xv = select i1 %xc, i32 {{O_EXCL}}, i32 0
  %f1 = or i32 %base, %av
  %f2 = or i32 %f1, %cv
  %f3 = or i32 %f2, %tv
  %flags = or i32 %f3, %xv
  %fd = call i32 (ptr, i32, ...) @open(ptr %path, i32 %flags, i32 438)
  %failed = icmp slt i32 %fd, 0
  br i1 %failed, label %failure, label %wrap
wrap:
  %wm = select i1 %write, ptr @__staple_fs_mode_w, ptr @__staple_fs_mode_r
  %mode = select i1 %rdwr, ptr @__staple_fs_mode_rw, ptr %wm
  %stream = call ptr @fdopen(i32 %fd, ptr %mode)
  %null = icmp eq ptr %stream, null
  br i1 %null, label %cleanup, label %done
cleanup:
  %ep = call ptr @{{ERRNO}}()
  %saved = load i32, ptr %ep
  %ignored = call i32 @close(i32 %fd)
  store i32 %saved, ptr %ep
  br label %failure
failure: ret ptr null
done: ret ptr %stream
}
define i32 @__staple_fs_stat(ptr %path, i32 %follow, ptr %kind, ptr %size, ptr %mtime, ptr %mode) {
  %buffer = alloca [{{STAT_SIZE}} x i8], align 8
  {{STAT_CALL}}
  %ok = icmp eq i32 %result, 0
  br i1 %ok, label %decode, label %done
decode:
  call void @__staple_fs_decode(ptr %buffer, ptr %kind, ptr %size, ptr %mtime, ptr %mode)
  br label %done
done:
  %error = call i32 @__staple_fs_errno()
  %negative = sub i32 0, %error
  %status = select i1 %ok, i32 0, i32 %negative
  ret i32 %status
}
define i32 @__staple_fs_fstat(ptr %stream, ptr %kind, ptr %size, ptr %mtime, ptr %mode) {
  %buffer = alloca [{{STAT_SIZE}} x i8], align 8
  %fd = call i32 @fileno(ptr %stream)
  {{FSTAT_CALL}}
  %ok = icmp eq i32 %result, 0
  br i1 %ok, label %decode, label %done
decode:
  call void @__staple_fs_decode(ptr %buffer, ptr %kind, ptr %size, ptr %mtime, ptr %mode)
  br label %done
done:
  %error = call i32 @__staple_fs_errno()
  %negative = sub i32 0, %error
  %status = select i1 %ok, i32 0, i32 %negative
  ret i32 %status
}
define internal void @__staple_fs_decode(ptr %buffer, ptr %kind, ptr %size, ptr %mtime, ptr %mode) {
  %mp = getelementptr i8, ptr %buffer, i64 {{MODE_OFFSET}}
  %m16 = load i16, ptr %mp, align 2
  %m = zext i16 %m16 to i32
  store i32 %m, ptr %mode
  %type = and i32 %m, 61440
  %regular = icmp eq i32 %type, 32768
  %directory = icmp eq i32 %type, 16384
  %symlink = icmp eq i32 %type, 40960
  %k1 = select i1 %symlink, i64 3, i64 0
  %k2 = select i1 %directory, i64 2, i64 %k1
  %k3 = select i1 %regular, i64 1, i64 %k2
  store i64 %k3, ptr %kind
  %sp = getelementptr i8, ptr %buffer, i64 {{SIZE_OFFSET}}
  %s = load i64, ptr %sp, align 8
  store i64 %s, ptr %size
  %tp = getelementptr i8, ptr %buffer, i64 {{TIME_OFFSET}}
  %seconds = load i64, ptr %tp, align 8
  %np = getelementptr i8, ptr %tp, i64 8
  %ns = load {{NSEC_TYPE}}, ptr %np, align 4
  {{NSEC_EXTEND}}
  %scaled = mul i64 %seconds, 1000000000
  %time = add i64 %scaled, %nanos
  store i64 %time, ptr %mtime
  ret void
}
define ptr @__staple_fs_dir_open(ptr %path) {
  %dir = call ptr @opendir(ptr %path)
  ret ptr %dir
}
define void @__staple_fs_dir_close(ptr %dir) {
  %ignored = call i32 @closedir(ptr %dir)
  ret void
}
define i32 @__staple_fs_dir_next(ptr %dir, ptr %name, ptr %length, ptr %kind) {
  %ep = call ptr @{{ERRNO}}()
  store i32 0, ptr %ep
  %entry = call ptr @{{READDIR}}(ptr %dir)
  %end = icmp eq ptr %entry, null
  br i1 %end, label %finished, label %found
finished:
  %error = load i32, ptr %ep
  %negative = sub i32 0, %error
  ret i32 %negative
found:
  %np = getelementptr i8, ptr %entry, i64 {{NAME_OFFSET}}
  %len = call {{SIZE}} @strlen(ptr %np)
  store ptr %np, ptr %name
  store {{SIZE}} %len, ptr %length
  %typep = getelementptr i8, ptr %entry, i64 {{TYPE_OFFSET}}
  %type = load i8, ptr %typep
  %regular = icmp eq i8 %type, 8
  %directory = icmp eq i8 %type, 4
  %symlink = icmp eq i8 %type, 10
  %k1 = select i1 %symlink, i64 3, i64 0
  %k2 = select i1 %directory, i64 2, i64 %k1
  %k3 = select i1 %regular, i64 1, i64 %k2
  store i64 %k3, ptr %kind
  ret i32 1
}
