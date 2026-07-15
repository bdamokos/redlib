const DEFAULT_KEEP_ALIVE: &str = "30s";

fn normalized_keep_alive(value: Option<String>) -> String {
	value
		.map(|value| value.trim().to_string())
		.filter(|value| !value.is_empty())
		.unwrap_or_else(|| DEFAULT_KEEP_ALIVE.to_string())
}

/// How long Ollama should retain an embedding model after Redlib's most
/// recent request. A short default keeps a burst fast while allowing the
/// runner to unload between the five-minute background refresh ticks.
pub fn keep_alive() -> String {
	normalized_keep_alive(std::env::var("REDLIB_OLLAMA_KEEP_ALIVE").ok())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn keep_alive_defaults_to_short_burst_window() {
		assert_eq!(normalized_keep_alive(None), "30s");
		assert_eq!(normalized_keep_alive(Some(" 2m ".to_string())), "2m");
		assert_eq!(normalized_keep_alive(Some("  ".to_string())), "30s");
	}
}
