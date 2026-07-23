fn main() {
	if let Err(error) = redlib::cache_sqlite::run_shadow_verifier() {
		eprintln!("cache_sqlite_shadow_verify: {error}");
		std::process::exit(1);
	}
}
