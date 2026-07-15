/// Ask glibc to return completely free heap pages to the operating system.
///
/// This is deliberately called only after allocation-heavy persistence work
/// has quiesced. Other platforms and allocators keep their normal behavior.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn trim_allocator() -> bool {
	// SAFETY: `malloc_trim` has no pointer arguments, is thread-safe in glibc,
	// and may be called while the process is otherwise running.
	unsafe { libc::malloc_trim(0) != 0 }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn trim_allocator() -> bool {
	false
}
