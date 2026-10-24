; Platform stdio bridge. The caller retains ownership of byte storage.
{{STREAMS}}
declare {{SIZE}} @fwrite(ptr, {{SIZE}}, {{SIZE}}, ptr)
declare i32 @fflush(ptr)
declare i32 @fgetc(ptr)
declare i32 @ferror(ptr)

define i32 @__staple_io_write(ptr %stream, ptr %bytes, {{SIZE}} %length) {
entry:
  %n = call {{SIZE}} @fwrite(ptr %bytes, {{SIZE}} 1, {{SIZE}} %length, ptr %stream)
  %ok = icmp eq {{SIZE}} %n, %length
  %status = select i1 %ok, i32 0, i32 1
  ret i32 %status
}

define i32 @__staple_io_flush(ptr %stream) {
  %status = call i32 @fflush(ptr %stream)
  ret i32 %status
}

; Nonnegative byte, -1 for EOF, -2 for an I/O error.
define i32 @__staple_io_read_byte(ptr %stream) {
entry:
  %byte = call i32 @fgetc(ptr %stream)
  %eof = icmp eq i32 %byte, -1
  br i1 %eof, label %check, label %done
check:
  %error = call i32 @ferror(ptr %stream)
  %failed = icmp ne i32 %error, 0
  %result = select i1 %failed, i32 -2, i32 -1
  ret i32 %result
done:
  ret i32 %byte
}
