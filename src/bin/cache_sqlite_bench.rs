fn main() {
	if let Err(error) = redlib::cache_sqlite_bench::run() {
		eprintln!("cache_sqlite_bench: {error}");
		std::process::exit(1);
	}
}
