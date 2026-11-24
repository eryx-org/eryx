;; Replaces unkernel's JSPI-based `wasm_switch_to` with wasm stack switching.
;;
;; Every kernel task runs as a continuation. `wasm_switch_to(prev, next,
;; is_new, prev_dead)` suspends to the scheduler loop in `run`, which parks
;; prev's continuation (unless it is dead) and resumes next's, or starts a
;; fresh one at `ret_from_fork(prev, next)` for a new task. The value a parked
;; task gets back from `wasm_switch_to` is the task that switched to it, as in
;; the JS host.
(module
  (type $void (func))
  (type $rff (func (param i64 i64)))
  (type $task (func (param i64)))
  (type $ct (cont $task))

  ;; task pointer -> slot in $conts (alloc = 1 to create one)
  (import "host" "slot" (func $slot (param i64 i32) (result i32)))
  (import "host" "unknown_task" (func $unknown (param i64)))

  (tag $sw (param i64 i64 i32 i32) (result i64))

  ;; [0] = unkernel's _start, [1] = its ret_from_fork; set by the host.
  (table $entries (export "entries") 2 funcref)
  ;; ponytail: fixed 64Ki task slots, grow the table if a workload needs more.
  (table $conts 65536 (ref null $ct))

  (global $new_prev (mut i64) (i64.const 0))
  (global $new_next (mut i64) (i64.const 0))

  (func (export "wasm_switch_to") (param i64 i64 i32 i32) (result i64)
    (suspend $sw (local.get 0) (local.get 1) (local.get 2) (local.get 3)))

  (func $start_task (type $task)
    (call_indirect $entries (type $void) (i32.const 0)))
  (func $fork_task (type $task)
    (call_indirect $entries (type $rff)
      (global.get $new_prev) (global.get $new_next) (i32.const 1)))
  (elem declare func $start_task $fork_task)

  (func (export "run")
    (local $k (ref null $ct))
    (local $v i64)
    (local $prev i64) (local $next i64) (local $is_new i32) (local $dead i32)
    (local $parked (ref null $ct))
    (local $s i32)
    (local.set $k (cont.new $ct (ref.func $start_task)))
    (loop $l
      (block $h (result i64 i64 i32 i32 (ref $ct))
        (resume $ct (on $sw $h) (local.get $v) (local.get $k))
        ;; a task returned: the kernel never does that
        (unreachable))
      (local.set $parked)
      (local.set $dead)
      (local.set $is_new)
      (local.set $next)
      (local.set $prev)
      (if (i32.eqz (local.get $dead))
        (then
          (table.set $conts
            (call $slot (local.get $prev) (i32.const 1))
            (local.get $parked))))
      (if (local.get $is_new)
        (then
          (global.set $new_prev (local.get $prev))
          (global.set $new_next (local.get $next))
          (local.set $k (cont.new $ct (ref.func $fork_task)))
          (local.set $v (i64.const 0)))
        (else
          (local.set $s (call $slot (local.get $next) (i32.const 0)))
          (local.set $k (table.get $conts (local.get $s)))
          (table.set $conts (local.get $s) (ref.null $ct))
          (if (ref.is_null (local.get $k))
            (then (call $unknown (local.get $next)) (unreachable)))
          (local.set $v (local.get $prev))))
      (br $l)))
)
